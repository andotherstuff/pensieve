//! Bounded sealed-archive replay for the isolated worker's inventory.
//!
//! The explicit rollout floor and next-segment cursor live in the existing sync
//! database. No full-history seed, notification-based completion or source cleanup.
//! One parent-owned session streams one segment; each step validates at most 256
//! frames, retaining only one bounded payload and a small ID/timestamp batch.

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use serde::{Deserialize, Serialize};

use super::SyncStateDb;
use crate::{DedupeIndex, Error, EventStatus, Result, SegmentWriter};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Maximum frames decoded in one replay executor turn.
pub const MAX_REPLAY_BATCH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u32,
    archive: PathBuf,
    prefix: String,
    floor: u64,
    next: u64,
}

/// One bounded replay step, not proof of remote relay coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayProgress {
    /// Frames validated and inserted this turn, including idempotent repeats.
    pub frames: usize,
    /// EOF was validated and the durable cursor advanced past this segment.
    pub segment_complete: bool,
}

/// Active replay reader. Dropping it leaves the cursor unchanged for safe replay.
pub struct InventoryReplay<'a> {
    _ownership: parking_lot::MutexGuard<'a, ()>,
    state: &'a SyncStateDb,
    dedupe: &'a DedupeIndex,
    writer: &'a SegmentWriter,
    cursor: Cursor,
    reader: Box<dyn Read + Send>,
    ended: bool,
}

impl<'a> InventoryReplay<'a> {
    /// Open exactly the next sealed segment. Initialize only with an explicit
    /// operator-chosen floor; later archive/prefix/floor changes fail closed.
    ///
    /// None means the writer is mutating the archive or the next segment has not
    /// checkpointed its seal yet. Busy writers cause no cursor/file access.
    /// Archive, prefix and dedupe must match this exact writer. A higher sealed segment
    /// with a missing predecessor is an error, never an instruction to skip it.
    /// Run after archive startup recovery, on a bounded blocking executor. Do not
    /// run the legacy inventory pruner while replay or retained jobs need history.
    pub fn begin(
        state: &'a SyncStateDb,
        dedupe: &'a DedupeIndex,
        writer: &'a SegmentWriter,
        archive: &Path,
        prefix: &str,
        floor: u64,
    ) -> Result<Option<Self>> {
        let Some(ready_before) = writer.inventory_source(dedupe, archive, prefix)? else {
            return Ok(None);
        };
        let ownership = state
            .replay_lock
            .try_lock()
            .ok_or_else(|| Error::Config("inventory replay already active".to_owned()))?;
        if writer.recovery_required()
            || prefix.is_empty()
            || prefix.len() > 64
            || !prefix
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(Error::Config(
                "invalid replay configuration or archive recovery required".to_owned(),
            ));
        }
        let archive = archive.canonicalize()?;
        let expected = Cursor {
            version: 1,
            archive: archive.clone(),
            prefix: prefix.to_owned(),
            floor,
            next: floor,
        };
        let cursor = match state.replay_cursor()? {
            Some(bytes) => {
                let cursor: Cursor =
                    serde_json::from_slice(&bytes).map_err(|e| Error::Validation(e.to_string()))?;
                if cursor.version != 1
                    || cursor.archive != archive
                    || cursor.prefix != prefix
                    || cursor.floor != floor
                    || cursor.next < floor
                {
                    return Err(Error::Config(
                        "replay cursor identity differs; preserve existing state".to_owned(),
                    ));
                }
                cursor
            }
            None => {
                save_cursor(state, &expected)?;
                expected
            }
        };
        let plain = archive.join(format!("{prefix}-{:09}.notepack", cursor.next));
        let gzip = archive.join(format!("{prefix}-{:09}.notepack.gz", cursor.next));
        if cursor.next >= ready_before {
            return Ok(None);
        }
        let reader: Box<dyn Read + Send> = match File::open(&plain) {
            Ok(file) => Box::new(BufReader::new(file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match File::open(&gzip) {
                Ok(file) => Box::new(MultiGzDecoder::new(BufReader::new(file))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Constant-memory discovery: no directory-wide vector/sort.
                    for entry in fs::read_dir(&archive)? {
                        let entry = entry?;
                        let name = entry.file_name();
                        let Some(name) = name.to_str() else { continue };
                        if let Some(number) = segment_number(name, prefix)
                            && number > cursor.next
                        {
                            return Err(Error::Validation(format!(
                                "missing replay segment {} before {number}",
                                cursor.next
                            )));
                        }
                    }
                    return Err(Error::Validation(format!(
                        "missing checkpointed replay segment {}",
                        cursor.next
                    )));
                }
                Err(error) => return Err(error.into()),
            },
            Err(error) => return Err(error.into()),
        };
        Ok(Some(Self {
            _ownership: ownership,
            state,
            dedupe,
            writer,
            cursor,
            reader,
            ended: false,
        }))
    }

    /// Process at most the supplied frame count; errors poison this reader and
    /// leave the segment cursor unchanged. Already inserted valid IDs are safe
    /// partial inventory, not coverage. Restart rereads only this unfinished segment.
    pub fn step(&mut self, max_frames: usize) -> Result<ReplayProgress> {
        if self.ended
            || max_frames == 0
            || max_frames > MAX_REPLAY_BATCH
            || self.writer.recovery_required()
        {
            return Err(Error::Config(
                "invalid replay step or archive recovery required".to_owned(),
            ));
        }
        self.ended = true;
        let mut batch = Vec::with_capacity(max_frames);
        let mut eof = false;
        for _ in 0..max_frames {
            let mut length = [0; 4];
            if self.reader.read(&mut length[..1])? == 0 {
                eof = true;
                break;
            }
            self.reader.read_exact(&mut length[1..])?;
            let length = u32::from_le_bytes(length) as usize;
            if length == 0 || length > MAX_FRAME_BYTES {
                return Err(Error::Validation(
                    "replay frame exceeds bounded decoder".to_owned(),
                ));
            }
            let mut payload = vec![0; length];
            self.reader.read_exact(&mut payload)?;
            let event = pensieve_parquet::CanonicalEvent::from_notepack(&payload)
                .map_err(|e| Error::Validation(e.to_string()))?;
            if self.dedupe.get_status(event.id())? != Some(EventStatus::Archived) {
                return Err(Error::Validation(
                    "sealed replay ID lacks durable archive marker".to_owned(),
                ));
            }
            batch.push((*event.id(), event.created_at()));
        }
        if self.writer.recovery_required() {
            return Err(Error::Validation("archive recovery required".to_owned()));
        }
        self.state
            .record_batch(batch.iter().map(|(id, timestamp)| (id, *timestamp)))?;
        if eof {
            // Refuse concurrent/stale readers rather than moving a cursor backward.
            let current = self
                .state
                .replay_cursor()?
                .ok_or_else(|| Error::Validation("missing replay cursor".to_owned()))?;
            let current: Cursor =
                serde_json::from_slice(&current).map_err(|e| Error::Validation(e.to_string()))?;
            if current != self.cursor {
                return Err(Error::Validation("stale replay session".to_owned()));
            }
            self.cursor.next =
                self.cursor.next.checked_add(1).ok_or_else(|| {
                    Error::Validation("replay segment counter overflow".to_owned())
                })?;
            save_cursor(self.state, &self.cursor)?;
        }
        self.ended = eof;
        Ok(ReplayProgress {
            frames: batch.len(),
            segment_complete: eof,
        })
    }
}

fn save_cursor(state: &SyncStateDb, cursor: &Cursor) -> Result<()> {
    let bytes = serde_json::to_vec(cursor).map_err(|e| Error::Serialization(e.to_string()))?;
    state.save_replay_cursor(&bytes)
}

fn segment_number(name: &str, prefix: &str) -> Option<u64> {
    name.strip_prefix(prefix)?
        .strip_prefix('-')?
        .strip_suffix(".notepack.gz")
        .or_else(|| {
            name.strip_prefix(prefix)?
                .strip_prefix('-')?
                .strip_suffix(".notepack")
        })?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::ArchivedWindow;
    use crate::{SegmentConfig, pack_nostr_event};
    use nostr_sdk::{EventBuilder, Keys, Timestamp};
    use std::io::Write;
    use std::sync::Arc;

    struct Harness {
        state: SyncStateDb,
        dedupe: Arc<DedupeIndex>,
        writer: SegmentWriter,
        archive: PathBuf,
    }

    impl Harness {
        fn new(root: &Path) -> Self {
            let state = SyncStateDb::open(root.join("sync")).unwrap();
            let dedupe = Arc::new(DedupeIndex::open(root.join("dedupe")).unwrap());
            let archive = root.join("archive");
            let writer = SegmentWriter::new(
                SegmentConfig {
                    output_dir: archive.clone(),
                    compress: false,
                    ..SegmentConfig::default()
                },
                None,
                Some(dedupe.clone()),
            )
            .unwrap();
            Self {
                state,
                dedupe,
                writer,
                archive,
            }
        }

        fn seal(&self, count: usize) -> Vec<[u8; 32]> {
            let mut ids = Vec::new();
            for n in 0..count {
                let event = EventBuilder::text_note(format!("event {n}"))
                    .custom_created_at(Timestamp::from(100))
                    .sign_with_keys(&Keys::generate())
                    .unwrap();
                self.writer
                    .write_reserved(
                        pack_nostr_event(&event).unwrap(),
                        self.dedupe.reserve(event.id.as_bytes()).unwrap().unwrap(),
                    )
                    .unwrap();
                ids.push(event.id.to_bytes());
            }
            self.writer.seal().unwrap().unwrap();
            ids.sort();
            ids
        }

        fn begin(&self) -> Result<Option<InventoryReplay<'_>>> {
            InventoryReplay::begin(
                &self.state,
                &self.dedupe,
                &self.writer,
                &self.archive,
                "segment",
                0,
            )
        }

        fn cursor(&self) -> Cursor {
            serde_json::from_slice(&self.state.replay_cursor().unwrap().unwrap()).unwrap()
        }
    }

    #[test]
    fn bounded_replay_reopens_without_skipping_partial_segment() {
        let dir = tempfile::tempdir().unwrap();
        let ids;
        {
            let h = Harness::new(dir.path());
            ids = h.seal(3);
            let mut replay = h.begin().unwrap().unwrap();
            assert!(h.begin().is_err());
            assert_eq!(
                replay.step(1).unwrap(),
                ReplayProgress {
                    frames: 1,
                    segment_complete: false
                }
            );
            assert_eq!(h.cursor().next, 0);
            // Drop before EOF: restart must repeat the segment, not skip two IDs.
        }
        let h = Harness::new(dir.path());
        let mut replay = h.begin().unwrap().unwrap();
        assert_eq!(replay.step(2).unwrap().frames, 2);
        assert_eq!(h.cursor().next, 0);
        assert_eq!(
            replay.step(2).unwrap(),
            ReplayProgress {
                frames: 1,
                segment_complete: true
            }
        );
        assert!(replay.step(2).is_err());
        drop(replay);
        assert_eq!(h.cursor().next, 1);
        assert!(h.begin().unwrap().is_none());
        assert_eq!(
            h.state.archived_window(100, 100, 3, &h.dedupe).unwrap(),
            ArchivedWindow::Complete(ids.iter().map(|id| (*id, 100)).collect())
        );
        drop(h);
        let h = Harness::new(dir.path());
        assert_eq!(h.cursor().next, 1);
        assert!(h.begin().unwrap().is_none());
    }

    #[test]
    fn abrupt_exit_replays_partial_segment_but_preserves_completed_cursor() {
        const ROOT: &str = "PENSIEVE_TEST_INVENTORY_CRASH_ROOT";
        const FINISH: &str = "PENSIEVE_TEST_INVENTORY_CRASH_FINISH";
        if let Some(root) = std::env::var_os(ROOT) {
            let h = Harness::new(Path::new(&root));
            let mut replay = h.begin().unwrap().unwrap();
            let finish = std::env::var_os(FINISH).is_some();
            let progress = replay.step(if finish { 4 } else { 1 }).unwrap();
            assert_eq!(progress.segment_complete, finish);
            std::process::exit(0); // Deliberately no RocksDB or replay destructors.
        }
        for finish in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let h = Harness::new(dir.path());
            h.seal(3);
            drop(h);
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", "sync::inventory::tests::abrupt_exit_replays_partial_segment_but_preserves_completed_cursor", "--nocapture"])
                .env(ROOT, dir.path()).env_remove(FINISH);
            if finish {
                child.env(FINISH, "1");
            }
            assert!(child.status().unwrap().success());
            let h = Harness::new(dir.path());
            assert_eq!(h.cursor().next, u64::from(finish));
            if !finish {
                let mut replay = h.begin().unwrap().unwrap();
                assert_eq!(replay.step(4).unwrap().frames, 3);
            }
            assert!(
                matches!(h.state.archived_window(100, 100, 3, &h.dedupe).unwrap(), ArchivedWindow::Complete(rows) if rows.len() == 3)
            );
        }
    }

    #[test]
    fn gap_and_changed_floor_fail_closed_without_advancing() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path());
        fs::write(h.archive.join("segment-000000001.notepack"), []).unwrap();
        drop(h);
        let h = Harness::new(dir.path());
        assert!(h.begin().is_err());
        let original = h.state.replay_cursor().unwrap();
        assert!(
            InventoryReplay::begin(&h.state, &h.dedupe, &h.writer, &h.archive, "segment", 1)
                .is_err()
        );
        assert_eq!(h.state.replay_cursor().unwrap(), original);
        assert_eq!(h.cursor().next, 0);
    }

    #[test]
    fn malformed_frame_poisoned_reader_never_advances_cursor() {
        for bytes in [
            vec![1, 0],
            vec![0, 0, 0, 0],
            (MAX_FRAME_BYTES as u32 + 1).to_le_bytes().to_vec(),
            vec![4, 0, 0, 0, 1],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let h = Harness::new(dir.path());
            let file = h.archive.join("segment-000000000.notepack");
            fs::write(&file, &bytes).unwrap();
            drop(h);
            let h = Harness::new(dir.path());
            let mut replay = h.begin().unwrap().unwrap();
            assert!(replay.step(1).is_err());
            assert!(replay.step(1).is_err());
            assert_eq!(h.cursor().next, 0);
            assert_eq!(fs::read(file).unwrap(), bytes);
        }
    }

    #[test]
    fn gzip_replay_and_archived_only_window_export_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path());
        let ids = h.seal(2);
        let plain = h.archive.join("segment-000000000.notepack");
        let gzip = h.archive.join("segment-000000000.notepack.gz");
        let mut encoder = flate2::write::GzEncoder::new(
            File::create(gzip).unwrap(),
            flate2::Compression::default(),
        );
        encoder.write_all(&fs::read(&plain).unwrap()).unwrap();
        encoder.finish().unwrap().sync_all().unwrap();
        fs::rename(plain, dir.path().join("preserved-source")).unwrap();
        let mut replay = h.begin().unwrap().unwrap();
        assert!(replay.step(3).unwrap().segment_complete);
        drop(replay);
        h.state.record(&[99; 32], 100).unwrap(); // unverified legacy inventory
        assert_eq!(
            h.state.archived_window(100, 100, 2, &h.dedupe).unwrap(),
            ArchivedWindow::TooDense
        );
        assert_eq!(
            h.state.archived_window(100, 100, 3, &h.dedupe).unwrap(),
            ArchivedWindow::Complete(ids.iter().map(|id| (*id, 100)).collect())
        );
        assert_eq!(
            h.state.archived_window(101, 102, 1, &h.dedupe).unwrap(),
            ArchivedWindow::Complete(vec![])
        );
        assert!(h.state.archived_window(101, 100, 1, &h.dedupe).is_err());
        assert!(h.state.archived_window(0, 100, 0, &h.dedupe).is_err());
        assert!(
            h.state
                .archived_window(0, 100, super::super::MAX_WINDOW_ITEMS + 1, &h.dedupe)
                .is_err()
        );
        // Existing prune implementation must never erase reserved cursor metadata.
        let cursor = h.state.replay_cursor().unwrap();
        h.state.prune_before(u64::MAX).unwrap();
        assert_eq!(h.state.replay_cursor().unwrap(), cursor);
    }

    #[test]
    fn seal_without_durable_marker_does_not_advance_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path());
        h.seal(1);
        let event = EventBuilder::text_note("unarchived replacement")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        let packed = pack_nostr_event(&event).unwrap();
        let mut bytes = (packed.data.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&packed.data);
        let file = h.archive.join("segment-000000000.notepack");
        fs::write(&file, &bytes).unwrap();
        let mut replay = h.begin().unwrap().unwrap();
        assert!(replay.step(2).is_err());
        assert_eq!(h.cursor().next, 0);
        assert_eq!(fs::read(file).unwrap(), bytes);
    }

    #[test]
    fn mismatched_source_or_dedupe_never_initializes_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(dir.path());
        h.seal(1);
        let empty = DedupeIndex::open(dir.path().join("empty-index")).unwrap();
        assert!(
            InventoryReplay::begin(&h.state, &empty, &h.writer, &h.archive, "segment", 0).is_err()
        );
        assert!(
            InventoryReplay::begin(&h.state, &h.dedupe, &h.writer, dir.path(), "segment", 0)
                .is_err()
        );
        assert!(
            InventoryReplay::begin(&h.state, &h.dedupe, &h.writer, &h.archive, "other", 0).is_err()
        );
        assert!(h.state.replay_cursor().unwrap().is_none());
        let without_dedupe = SegmentWriter::new(
            SegmentConfig {
                output_dir: h.archive.clone(),
                compress: false,
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        assert!(
            InventoryReplay::begin(
                &h.state,
                &h.dedupe,
                &without_dedupe,
                &h.archive,
                "segment",
                0
            )
            .is_err()
        );
        assert!(h.state.replay_cursor().unwrap().is_none());
        assert!(h.begin().unwrap().is_some());
    }
}
