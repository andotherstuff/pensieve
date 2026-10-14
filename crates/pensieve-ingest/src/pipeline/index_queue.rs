//! Durable journal of sealed segments not yet fully indexed into ClickHouse.
//!
//! The segment writer records each segment here when it is sealed, before the
//! ClickHouse indexer sees it. The indexer records its progress before every
//! insert batch and removes the entry once the whole segment is in ClickHouse.
//! An entry therefore survives any crash, kill or early shutdown, and the next
//! start resumes the segment from its recorded offset.
//!
//! # Why progress is tracked
//!
//! `events_local` is a `ReplacingMergeTree`, but several materialized views
//! attached to it (reaction/comment/repost counts, daily user stats, ...) are
//! not duplicate-safe. Re-inserting a row that is already present
//! double-counts it downstream. Progress lets recovery skip every batch known
//! to be committed and check for existing IDs only where the outcome is
//! unknown: the one batch that may have been in flight, or the whole segment
//! for entries without progress (written by older binaries).
//!
//! # File format
//!
//! One entry per line, rewritten atomically (temp file, fsync, rename):
//!
//! ```text
//! v1\t<committed>\t<verify_end or *>\t<segment path>
//! ```
//!
//! A line without the `v1` prefix is a bare segment path from older binaries
//! and has unknown progress.
//!
//! # Backlog age
//!
//! The journal also knows how long its oldest entry has waited
//! ([`IndexQueue::oldest_pending_age`]). Entries sealed by this process are
//! timed from when they were journaled; entries from a previous run use the
//! segment file's modification time, which is when it was sealed (or
//! compressed, a few seconds later).

use crate::{Error, Result};
use metrics::gauge;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const LINE_PREFIX: &str = "v1";

/// How far a segment has been indexed.
///
/// Events are counted in segment order (as returned by the segment reader).
/// Events before `committed` are known to be in ClickHouse. Events in
/// `committed..verify_end` may or may not be, so they must be checked against
/// ClickHouse before insert. Events at or after `verify_end` were never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexProgress {
    /// Number of leading events known to be in ClickHouse.
    pub committed: usize,
    /// End (exclusive) of the range whose ClickHouse state is unknown.
    pub verify_end: usize,
}

impl IndexProgress {
    /// Progress for a segment whose state is entirely unknown: every event
    /// must be checked before insert.
    pub const UNKNOWN: Self = Self {
        committed: 0,
        verify_end: usize::MAX,
    };

    /// Whether the batch starting at `start` needs an existence check.
    pub fn needs_verify(&self, start: usize) -> bool {
        start < self.verify_end
    }
}

#[derive(Debug, Clone)]
struct Entry {
    path: PathBuf,
    progress: IndexProgress,
    from_previous_run: bool,
    /// When the segment started waiting for ClickHouse.
    pending_since: SystemTime,
}

/// Durable, process-shared journal of segments pending ClickHouse indexing.
#[derive(Debug)]
pub struct IndexQueue {
    path: PathBuf,
    entries: Mutex<BTreeMap<PathBuf, Entry>>,
}

impl IndexQueue {
    /// Open (or create on first write) the journal at `path`.
    ///
    /// Entries already on disk are marked as belonging to a previous run; see
    /// [`IndexQueue::previous_run`].
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut entries = BTreeMap::new();
        let opened_at = SystemTime::now();
        match fs::read_to_string(&path) {
            Ok(contents) => {
                for line in contents.lines() {
                    let Some((segment, progress)) = parse_line(line) else {
                        continue;
                    };
                    let key = segment_key(&segment);
                    let entry = entries.entry(key).or_insert_with(|| Entry {
                        path: segment.clone(),
                        progress,
                        from_previous_run: true,
                        pending_since: sealed_at(&segment).unwrap_or(opened_at),
                    });
                    // Duplicate lines (older binaries appended blindly): keep
                    // the most conservative progress.
                    entry.progress = conservative(entry.progress, progress);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let queue = Self {
            path,
            entries: Mutex::new(entries),
        };
        queue.publish_depth(&queue.entries.lock());
        Ok(queue)
    }

    /// Path of the journal file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record a newly sealed segment. Existing progress is left untouched.
    pub fn record_sealed(&self, segment: &Path) -> Result<()> {
        format_line(segment, IndexProgress::default())?;
        let mut entries = self.entries.lock();
        let key = segment_key(segment);
        if entries.contains_key(&key) {
            return Ok(());
        }
        entries.insert(
            key,
            Entry {
                path: segment.to_path_buf(),
                progress: IndexProgress::default(),
                from_previous_run: false,
                pending_since: SystemTime::now(),
            },
        );
        self.persist(&entries)
    }

    /// Durably record indexing progress for a segment.
    pub fn record_progress(&self, segment: &Path, progress: IndexProgress) -> Result<()> {
        // Validate before mutating so a bad path never poisons later writes.
        format_line(segment, progress)?;
        let mut entries = self.entries.lock();
        let entry = entries
            .entry(segment_key(segment))
            .or_insert_with(|| Entry {
                path: segment.to_path_buf(),
                progress,
                from_previous_run: false,
                pending_since: SystemTime::now(),
            });
        entry.path = segment.to_path_buf();
        entry.progress = progress;
        self.persist(&entries)
    }

    /// Remove a fully indexed segment from the journal.
    pub fn complete(&self, segment: &Path) -> Result<()> {
        let mut entries = self.entries.lock();
        if entries.remove(&segment_key(segment)).is_none() {
            return Ok(());
        }
        self.persist(&entries)
    }

    /// Recorded progress for a segment, if it is in the journal.
    pub fn progress(&self, segment: &Path) -> Option<IndexProgress> {
        self.entries
            .lock()
            .get(&segment_key(segment))
            .map(|entry| entry.progress)
    }

    /// Entries that were already on disk when the journal was opened, in
    /// segment order. Segments sealed by this process arrive on the indexer
    /// channel instead.
    pub fn previous_run(&self) -> Vec<(PathBuf, IndexProgress)> {
        self.entries
            .lock()
            .values()
            .filter(|entry| entry.from_previous_run)
            .map(|entry| (entry.path.clone(), entry.progress))
            .collect()
    }

    /// Number of segments in the journal.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Whether the journal is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    /// How long the longest-waiting segment has been pending, or `None` when
    /// the journal is empty.
    pub fn oldest_pending_age(&self) -> Option<Duration> {
        oldest_age(&self.entries.lock())
    }

    /// Publish the backlog gauges: `clickhouse_index_queue_segments` and
    /// `clickhouse_index_oldest_pending_seconds` (0 when empty).
    pub fn publish_metrics(&self) {
        self.publish_depth(&self.entries.lock());
    }

    fn persist(&self, entries: &BTreeMap<PathBuf, Entry>) -> Result<()> {
        self.publish_depth(entries);
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if entries.is_empty() {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            File::open(parent)?.sync_all()?;
            return Ok(());
        }

        let mut contents = String::new();
        for entry in entries.values() {
            contents.push_str(&format_line(&entry.path, entry.progress)?);
            contents.push('\n');
        }
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let mut file = File::create(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &self.path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    fn publish_depth(&self, entries: &BTreeMap<PathBuf, Entry>) {
        gauge!("clickhouse_index_queue_segments").set(entries.len() as f64);
        let age = oldest_age(entries).unwrap_or_default();
        gauge!("clickhouse_index_oldest_pending_seconds").set(age.as_secs_f64());
    }
}

fn oldest_age(entries: &BTreeMap<PathBuf, Entry>) -> Option<Duration> {
    let oldest = entries.values().map(|entry| entry.pending_since).min()?;
    Some(SystemTime::now().duration_since(oldest).unwrap_or_default())
}

/// Seal time of a journaled segment from a previous run: its file's mtime.
fn sealed_at(segment: &Path) -> Option<SystemTime> {
    fs::metadata(resolve_segment(segment)?)
        .ok()?
        .modified()
        .ok()
}

/// Locate a journaled segment on disk. The writer journals the uncompressed
/// `.notepack` path at seal time; background compression later replaces it
/// with `.notepack.gz` (renamed into place only once complete), so prefer the
/// compressed file and fall back to the uncompressed one.
pub fn resolve_segment(path: &Path) -> Option<PathBuf> {
    let key = segment_key(path);
    let mut compressed = key.clone().into_os_string();
    compressed.push(".notepack.gz");
    let mut uncompressed = key.into_os_string();
    uncompressed.push(".notepack");
    [PathBuf::from(compressed), PathBuf::from(uncompressed)]
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// Identity of a segment independent of compression: the path without its
/// `.notepack` / `.notepack.gz` suffix.
fn segment_key(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    let stem = raw
        .strip_suffix(".notepack.gz")
        .or_else(|| raw.strip_suffix(".notepack"))
        .unwrap_or(&raw);
    PathBuf::from(stem)
}

fn conservative(a: IndexProgress, b: IndexProgress) -> IndexProgress {
    IndexProgress {
        committed: a.committed.min(b.committed),
        verify_end: a.verify_end.max(b.verify_end),
    }
}

fn parse_line(line: &str) -> Option<(PathBuf, IndexProgress)> {
    let line = line.trim_end_matches('\r');
    if line.trim().is_empty() {
        return None;
    }
    let mut fields = line.splitn(4, '\t');
    if fields.next() == Some(LINE_PREFIX)
        && let (Some(committed), Some(verify_end), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        && let Ok(committed) = committed.parse::<usize>()
        && !path.is_empty()
    {
        let verify_end = match verify_end {
            "*" => Some(usize::MAX),
            value => value.parse::<usize>().ok(),
        };
        if let Some(verify_end) = verify_end {
            return Some((
                PathBuf::from(path),
                IndexProgress {
                    committed,
                    verify_end,
                },
            ));
        }
    }
    // Bare path from an older binary, or a line we cannot interpret: index it
    // again, checking every event.
    Some((PathBuf::from(line.trim()), IndexProgress::UNKNOWN))
}

fn format_line(path: &Path, progress: IndexProgress) -> Result<String> {
    let path = path.to_str().ok_or_else(|| {
        Error::Config(format!(
            "segment path is not valid UTF-8: {}",
            path.display()
        ))
    })?;
    if path.contains(['\n', '\r', '\t']) {
        return Err(Error::Config(format!(
            "segment path contains a control character: {path:?}"
        )));
    }
    let verify_end = if progress.verify_end == usize::MAX {
        "*".to_string()
    } else {
        progress.verify_end.to_string()
    };
    Ok(format!(
        "{LINE_PREFIX}\t{}\t{verify_end}\t{path}",
        progress.committed
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_in(dir: &Path) -> IndexQueue {
        IndexQueue::open(dir.join(".clickhouse_reindex_queue")).unwrap()
    }

    #[test]
    fn sealed_progress_and_completion_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("segment-000000001.notepack");
        let b = dir.path().join("segment-000000002.notepack");

        let queue = queue_in(dir.path());
        assert!(queue.previous_run().is_empty());
        queue.record_sealed(&a).unwrap();
        queue.record_sealed(&b).unwrap();
        // The indexer sees the compressed path; it maps to the same entry.
        let a_gz = dir.path().join("segment-000000001.notepack.gz");
        let progress = IndexProgress {
            committed: 20_000,
            verify_end: 30_000,
        };
        queue.record_progress(&a_gz, progress).unwrap();
        assert_eq!(queue.len(), 2);
        // Sealing again must not reset progress.
        queue.record_sealed(&a).unwrap();
        assert_eq!(queue.progress(&a), Some(progress));
        // Entries written in this run are not "previous run" work.
        assert!(queue.previous_run().is_empty());
        drop(queue);

        let reopened = queue_in(dir.path());
        assert_eq!(
            reopened.previous_run(),
            vec![
                (a_gz.clone(), progress),
                (b.clone(), IndexProgress::default())
            ]
        );
        reopened.complete(&a).unwrap();
        reopened.complete(&b).unwrap();
        assert!(reopened.is_empty());
        assert!(!reopened.path().exists(), "empty journal removes the file");
        drop(reopened);
        assert!(queue_in(dir.path()).previous_run().is_empty());
    }

    #[test]
    fn legacy_bare_paths_need_full_verification() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join(".clickhouse_reindex_queue");
        let legacy = dir.path().join("segment-000000007.notepack.gz");
        // Older binaries appended the same path once per failure.
        fs::write(
            &journal,
            format!("{}\n\n{}\n", legacy.display(), legacy.display()),
        )
        .unwrap();

        let queue = IndexQueue::open(&journal).unwrap();
        assert_eq!(
            queue.previous_run(),
            vec![(legacy.clone(), IndexProgress::UNKNOWN)]
        );
        assert!(IndexProgress::UNKNOWN.needs_verify(1_000_000));

        // Rewritten in the current format, and still unknown after reopen.
        queue
            .complete(Path::new("/elsewhere/segment-1.notepack"))
            .unwrap();
        queue
            .record_sealed(&dir.path().join("segment-000000008.notepack"))
            .unwrap();
        let contents = fs::read_to_string(&journal).unwrap();
        assert!(contents.lines().all(|line| line.starts_with("v1\t")));
        let reopened = IndexQueue::open(&journal).unwrap();
        assert_eq!(reopened.progress(&legacy), Some(IndexProgress::UNKNOWN));
    }

    #[test]
    fn duplicate_lines_keep_most_conservative_progress() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("queue");
        let segment = dir.path().join("segment-000000003.notepack.gz");
        fs::write(
            &journal,
            format!(
                "v1\t30000\t30000\t{0}\nv1\t10000\t20000\t{0}\n",
                segment.display()
            ),
        )
        .unwrap();
        let queue = IndexQueue::open(&journal).unwrap();
        assert_eq!(
            queue.progress(&segment),
            Some(IndexProgress {
                committed: 10_000,
                verify_end: 30_000,
            })
        );
    }

    #[test]
    fn needs_verify_covers_only_the_unknown_range() {
        let graceful = IndexProgress {
            committed: 20_000,
            verify_end: 20_000,
        };
        assert!(!graceful.needs_verify(20_000));
        let in_flight = IndexProgress {
            committed: 20_000,
            verify_end: 30_000,
        };
        assert!(in_flight.needs_verify(20_000));
        assert!(!in_flight.needs_verify(30_000));
        assert!(!IndexProgress::default().needs_verify(0));
    }

    #[test]
    fn resolve_prefers_compressed_segment() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("segment-000000004.notepack");
        let gz = dir.path().join("segment-000000004.notepack.gz");
        assert_eq!(resolve_segment(&plain), None);
        fs::write(&plain, b"").unwrap();
        assert_eq!(resolve_segment(&gz), Some(plain.clone()));
        fs::write(&gz, b"").unwrap();
        assert_eq!(resolve_segment(&plain), Some(gz.clone()));
        fs::remove_file(&plain).unwrap();
        assert_eq!(resolve_segment(&plain), Some(gz));
    }

    #[test]
    fn oldest_pending_age_tracks_the_longest_waiting_entry() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join(".clickhouse_reindex_queue");
        let old = dir.path().join("segment-000000010.notepack.gz");
        fs::write(&old, b"").unwrap();
        let sealed = SystemTime::now() - Duration::from_secs(3_600);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(sealed)
            .unwrap();
        // Journaled by a previous run, without its compression suffix.
        fs::write(
            &journal,
            format!(
                "v1\t0\t0\t{}\n",
                dir.path().join("segment-000000010.notepack").display()
            ),
        )
        .unwrap();

        let queue = IndexQueue::open(&journal).unwrap();
        let age = queue.oldest_pending_age().unwrap();
        assert!(
            (3_599..3_700).contains(&age.as_secs()),
            "previous-run entries are aged from the segment's mtime: {age:?}"
        );

        // A newer seal does not hide the stranded one.
        let new = dir.path().join("segment-000000011.notepack");
        queue.record_sealed(&new).unwrap();
        assert!(queue.oldest_pending_age().unwrap() >= age);

        queue.complete(&old).unwrap();
        assert!(queue.oldest_pending_age().unwrap() < Duration::from_secs(60));
        queue.complete(&new).unwrap();
        assert_eq!(queue.oldest_pending_age(), None);
    }

    #[test]
    fn rejects_paths_that_would_corrupt_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue_in(dir.path());
        let bad = dir.path().join("segment\n-1.notepack");
        assert!(queue.record_sealed(&bad).is_err());
        assert!(queue.is_empty());
        queue
            .record_sealed(&dir.path().join("segment-000000005.notepack"))
            .unwrap();
    }
}
