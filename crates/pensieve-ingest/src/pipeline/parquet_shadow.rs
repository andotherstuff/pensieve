//! Derived Parquet shadow sink for durably sealed notepack segments.
//!
//! The sink is deliberately downstream of [`super::SegmentWriter`]. Failure here
//! never changes notepack archival or deduplication state.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Receiver;
use metrics::{counter, gauge};
use pensieve_lake::{
    CampaignConfig, Inventory, LocalObjectStore, Publisher, S3Publisher, S3PublisherConfig,
    run_notepack_work_unit,
};

use super::SealedSegment;

const REPLAY_POLICY_SETTING: &str = "parquet_shadow.replay_policy.v1";

/// Publication target for the live shadow sink.
#[derive(Clone, Debug)]
pub enum ParquetShadowPublisher {
    /// Immutable local filesystem namespace.
    Local {
        /// Root of the namespace.
        root: PathBuf,
    },
    /// AWS S3 or an S3-compatible provider.
    S3(S3PublisherConfig),
}

/// Durable live shadow configuration.
#[derive(Clone, Debug)]
pub struct ParquetShadowConfig {
    /// SQLite work journal and active-object inventory.
    pub state_db: PathBuf,
    /// Canonical conversion settings.
    pub campaign: CampaignConfig,
    /// Immutable publication target.
    pub publisher: ParquetShadowPublisher,
    /// Directory scanned for sealed work units before consuming live notifications.
    pub replay_dir: Option<PathBuf>,
    /// Inclusive segment-number floor for startup replay.
    ///
    /// This lets a new live shadow take ownership of future segments without
    /// replaying the pre-existing historical archive. The effective replay
    /// policy is persisted in the inventory and cannot drift across restarts.
    pub replay_from_segment: Option<u64>,
}

/// Start the shadow worker and wait until its inventory and publisher initialize.
///
/// Historical replay happens inside the returned worker. Dropping every sender
/// closes the queue; joining the handle then waits for all sealed inputs to
/// finish publication.
pub fn start_parquet_shadow(
    config: ParquetShadowConfig,
    receiver: Receiver<SealedSegment>,
) -> Result<JoinHandle<()>> {
    let (ready_sender, ready_receiver) = crossbeam_channel::bounded(1);
    let handle = std::thread::Builder::new()
        .name("parquet-shadow".to_string())
        .spawn(move || {
            let initialized = ShadowWorker::new(config);
            match initialized {
                Ok(mut worker) => {
                    let _ = ready_sender.send(Ok(()));
                    worker.run(receiver);
                }
                Err(error) => {
                    let _ = ready_sender.send(Err(error.to_string()));
                }
            }
        })
        .context("failed to spawn Parquet shadow worker")?;

    match ready_receiver.recv() {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(error)) => {
            let _ = handle.join();
            Err(anyhow!(error))
        }
        Err(error) => {
            let _ = handle.join();
            Err(anyhow!(
                "Parquet shadow worker exited during startup: {error}"
            ))
        }
    }
}

struct ShadowWorker {
    inventory: Inventory,
    publisher: Box<dyn Publisher>,
    config: ParquetShadowConfig,
}

impl ShadowWorker {
    fn new(config: ParquetShadowConfig) -> Result<Self> {
        let mut inventory = Inventory::open(&config.state_db)
            .with_context(|| format!("failed to open {}", config.state_db.display()))?;
        let replay_policy = replay_policy(&config);
        let policy_is_recorded = inventory
            .setting(REPLAY_POLICY_SETTING)
            .context("failed to load durable Parquet shadow replay policy")?
            .is_some();
        validate_replay_floor(&config, policy_is_recorded)?;
        ensure_inventory_settings(&mut inventory, &config, &replay_policy)?;
        let publisher: Box<dyn Publisher> = match &config.publisher {
            ParquetShadowPublisher::Local { root } => Box::new(
                LocalObjectStore::new(root)
                    .with_context(|| format!("failed to open local lake at {}", root.display()))?,
            ),
            ParquetShadowPublisher::S3(s3_config) => Box::new(
                S3Publisher::from_environment(s3_config.clone())
                    .context("failed to initialize S3 publisher")?,
            ),
        };
        Ok(Self {
            inventory,
            publisher,
            config,
        })
    }

    fn run(&mut self, receiver: Receiver<SealedSegment>) {
        gauge!("parquet_shadow_running").set(1.0);
        if let Some(replay_dir) = self.config.replay_dir.clone() {
            match discover_sealed_segments(&replay_dir, self.config.replay_from_segment) {
                Ok(paths) => {
                    tracing::info!(
                        path = %replay_dir.display(),
                        segments = paths.len(),
                        replay_from_segment = ?self.config.replay_from_segment,
                        "replaying sealed segments through Parquet shadow"
                    );
                    for path in paths {
                        self.process(&path, true);
                    }
                }
                Err(error) => {
                    counter!("parquet_shadow_failures_total", "phase" => "discovery").increment(1);
                    tracing::error!(
                        path = %replay_dir.display(),
                        error = %error,
                        "failed to discover sealed segments for Parquet shadow replay"
                    );
                }
            }
        }

        for sealed in receiver {
            self.process(&sealed.path, false);
        }
        gauge!("parquet_shadow_running").set(0.0);
        tracing::info!("Parquet shadow worker stopped");
    }

    fn process(&mut self, path: &Path, replayed: bool) {
        match run_notepack_work_unit(
            &mut self.inventory,
            self.publisher.as_ref(),
            path,
            &self.config.campaign,
        ) {
            Ok(summary) => {
                counter!("parquet_shadow_work_units_total", "result" => "published").increment(1);
                tracing::info!(
                    path = %path.display(),
                    work_unit_id = %summary.work_unit_id,
                    input_events = summary.input_events,
                    output_rows = summary.output_rows,
                    rejected_events = summary.rejected_events,
                    parquet_objects = summary.parquet_objects,
                    resumed = summary.resumed,
                    replayed,
                    "Parquet shadow work unit published"
                );
            }
            Err(error) => {
                counter!("parquet_shadow_work_units_total", "result" => "failed").increment(1);
                counter!("parquet_shadow_failures_total", "phase" => "work_unit").increment(1);
                tracing::error!(
                    path = %path.display(),
                    error = %error,
                    replayed,
                    "Parquet shadow work unit failed; notepack archive remains authoritative"
                );
            }
        }
    }
}

fn ensure_inventory_settings(
    inventory: &mut Inventory,
    config: &ParquetShadowConfig,
    replay_policy: &str,
) -> Result<()> {
    let publisher = match &config.publisher {
        ParquetShadowPublisher::Local { root } => {
            format!("local:{}", root.to_string_lossy())
        }
        ParquetShadowPublisher::S3(s3) => format!(
            "s3:bucket={};region={};endpoint={};force_path_style={}",
            s3.bucket,
            s3.region.as_deref().unwrap_or_default(),
            s3.endpoint_url.as_deref().unwrap_or_default(),
            s3.force_path_style
        ),
    };
    let settings = [
        (REPLAY_POLICY_SETTING, replay_policy.to_owned()),
        ("parquet_shadow.publisher.v1", publisher),
        (
            "parquet_shadow.object_prefix.v1",
            config.campaign.object_prefix.clone(),
        ),
        (
            "parquet_shadow.target_uncompressed_bytes.v1",
            config.campaign.target_uncompressed_bytes.to_string(),
        ),
        (
            "parquet_shadow.max_event_bytes.v1",
            config.campaign.max_event_bytes.to_string(),
        ),
        (
            "parquet_shadow.staging_dir.v1",
            config.campaign.staging_dir.to_string_lossy().into_owned(),
        ),
    ];
    for (key, value) in settings {
        inventory
            .ensure_setting(key, &value)
            .with_context(|| format!("Parquet shadow setting {key} differs from its inventory"))?;
    }
    Ok(())
}

fn replay_policy(config: &ParquetShadowConfig) -> String {
    match (config.replay_dir.is_some(), config.replay_from_segment) {
        (false, _) => "disabled".to_owned(),
        (true, Some(segment)) => format!("from-segment:{segment}"),
        (true, None) => "all".to_owned(),
    }
}

fn validate_replay_floor(config: &ParquetShadowConfig, policy_is_recorded: bool) -> Result<()> {
    let (Some(directory), Some(floor)) = (&config.replay_dir, config.replay_from_segment) else {
        return Ok(());
    };
    let next_segment = next_segment_number(directory).with_context(|| {
        format!(
            "failed to inspect {} for the Parquet shadow replay floor",
            directory.display()
        )
    })?;
    if (!policy_is_recorded && floor != next_segment)
        || (policy_is_recorded && next_segment < floor)
    {
        let requirement = if policy_is_recorded {
            "at or below the next segment number"
        } else {
            "exactly the next segment number on first activation"
        };
        anyhow::bail!("Parquet shadow replay floor {floor} must be {requirement} {next_segment}");
    }
    Ok(())
}

fn next_segment_number(directory: &Path) -> std::io::Result<u64> {
    let mut highest = None;
    if directory.exists() {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_file()
                && let Some(segment) = segment_number(&path)
            {
                highest = Some(highest.map_or(segment, |current: u64| current.max(segment)));
            }
        }
    }
    highest.map_or(Ok(0), |segment| {
        segment.checked_add(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "segment number space exhausted",
            )
        })
    })
}

fn discover_sealed_segments(
    directory: &Path,
    replay_from_segment: Option<u64>,
) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if !directory.exists() {
        return Ok(paths);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_file()
            && is_sealed_notepack(&path)
            && segment_is_at_or_after(&path, replay_from_segment)
            && !has_compressed_replacement(&path)
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn segment_is_at_or_after(path: &Path, replay_from_segment: Option<u64>) -> bool {
    let Some(floor) = replay_from_segment else {
        return true;
    };
    segment_number(path).is_some_and(|segment| segment >= floor)
}

fn segment_number(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("segment-")?;
    let number = rest
        .strip_suffix(".notepack.gz")
        .or_else(|| rest.strip_suffix(".notepack"))
        .or_else(|| rest.strip_suffix(".notepack.open"))?;
    number.parse().ok()
}

fn has_compressed_replacement(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".notepack") && path.with_extension("notepack.gz").is_file()
}

fn is_sealed_notepack(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".notepack") || name.ends_with(".notepack.gz")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SegmentConfig, SegmentWriter, pack_nostr_event};
    use nostr_sdk::{EventBuilder, Keys, Kind};

    #[test]
    fn discovery_excludes_open_segments() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "segment-000000001.notepack.open",
            "segment-000000002.notepack",
            "segment-000000003.notepack.gz",
            "segment-000000004.notepack",
            "segment-000000004.notepack.gz",
            "unrelated.txt",
        ] {
            fs::write(directory.path().join(name), []).unwrap();
        }

        let found = discover_sealed_segments(directory.path(), None).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "segment-000000002.notepack",
                "segment-000000003.notepack.gz",
                "segment-000000004.notepack.gz"
            ]
        );
    }

    #[test]
    fn discovery_replay_floor_is_inclusive_and_ignores_historical_segments() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "segment-000000040.notepack.gz",
            "segment-000000041.notepack",
            "segment-000000042.notepack",
            "segment-000000042.notepack.gz",
            "segment-000000043.notepack.gz",
            "other-000000100.notepack.gz",
        ] {
            fs::write(directory.path().join(name), []).unwrap();
        }

        let found = discover_sealed_segments(directory.path(), Some(42)).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "segment-000000042.notepack.gz",
                "segment-000000043.notepack.gz"
            ]
        );
    }

    #[test]
    fn first_replay_floor_must_match_next_segment_and_then_remains_durable() {
        let directory = tempfile::tempdir().unwrap();
        let segment_dir = directory.path().join("segments");
        fs::create_dir(&segment_dir).unwrap();
        fs::write(segment_dir.join("segment-000000041.notepack.gz"), []).unwrap();
        let state_db = directory.path().join("inventory.sqlite");
        let config = |floor| ParquetShadowConfig {
            state_db: state_db.clone(),
            campaign: CampaignConfig::new(directory.path().join("staging")),
            publisher: ParquetShadowPublisher::Local {
                root: directory.path().join("lake"),
            },
            replay_dir: Some(segment_dir.clone()),
            replay_from_segment: Some(floor),
        };

        assert!(ShadowWorker::new(config(41)).is_err());
        ShadowWorker::new(config(42)).expect("next segment is a safe first floor");

        fs::write(segment_dir.join("segment-000000100.notepack.open"), []).unwrap();
        ShadowWorker::new(config(42)).expect("recorded floor survives later segments");
        assert!(ShadowWorker::new(config(43)).is_err());
    }

    #[test]
    fn restart_replays_live_segment_without_replaying_historical_segment() {
        let directory = tempfile::tempdir().unwrap();
        let segment_dir = directory.path().join("segments");
        let lake_dir = directory.path().join("lake");
        let inventory_path = directory.path().join("inventory.sqlite");
        fs::create_dir(&segment_dir).unwrap();
        fs::write(segment_dir.join("segment-000000041.notepack.gz"), []).unwrap();
        let config = ParquetShadowConfig {
            state_db: inventory_path.clone(),
            campaign: CampaignConfig::new(directory.path().join("staging")),
            publisher: ParquetShadowPublisher::Local { root: lake_dir },
            replay_dir: Some(segment_dir.clone()),
            replay_from_segment: Some(42),
        };

        let (sender, receiver) = crossbeam_channel::unbounded();
        drop(sender);
        start_parquet_shadow(config.clone(), receiver)
            .unwrap()
            .join()
            .unwrap();

        let segment_writer = SegmentWriter::new(
            SegmentConfig {
                output_dir: segment_dir,
                compress: false,
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        let event = EventBuilder::new(Kind::TextNote, "restart replay")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        segment_writer
            .write(pack_nostr_event(&event).unwrap())
            .unwrap();
        let sealed = segment_writer.seal().unwrap().expect("sealed segment");
        assert_eq!(sealed.segment_number, 42);
        drop(segment_writer);

        let (sender, receiver) = crossbeam_channel::unbounded();
        drop(sender);
        start_parquet_shadow(config, receiver)
            .unwrap()
            .join()
            .unwrap();

        let inventory = Inventory::open(inventory_path).unwrap();
        let active = inventory.active_raw_objects().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].row_count, 1);
    }

    #[test]
    fn sealed_segment_is_published_after_notepack_seal() {
        let directory = tempfile::tempdir().unwrap();
        let segment_dir = directory.path().join("segments");
        let lake_dir = directory.path().join("lake");
        let inventory_path = directory.path().join("inventory.sqlite");
        let (sender, receiver) = crossbeam_channel::unbounded();
        let segment_writer = SegmentWriter::new(
            SegmentConfig {
                output_dir: segment_dir,
                compress: false,
                ..Default::default()
            },
            Some(sender),
            None,
        )
        .unwrap();
        let shadow_handle = start_parquet_shadow(
            ParquetShadowConfig {
                state_db: inventory_path.clone(),
                campaign: CampaignConfig {
                    staging_dir: directory.path().join("staging"),
                    object_prefix: "test/v1".to_string(),
                    target_uncompressed_bytes: 1024,
                    max_event_bytes: 1024 * 1024,
                },
                publisher: ParquetShadowPublisher::Local { root: lake_dir },
                replay_dir: None,
                replay_from_segment: None,
            },
            receiver,
        )
        .unwrap();

        let event = EventBuilder::new(Kind::TextNote, "shadow event")
            .sign_with_keys(&Keys::generate())
            .unwrap();
        segment_writer
            .write(pack_nostr_event(&event).unwrap())
            .unwrap();
        segment_writer.seal().unwrap();
        drop(segment_writer);
        shadow_handle.join().unwrap();

        let inventory = Inventory::open(inventory_path).unwrap();
        let active = inventory.active_raw_objects().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].row_count, 1);
    }
}
