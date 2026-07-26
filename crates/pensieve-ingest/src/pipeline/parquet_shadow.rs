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
        let inventory = Inventory::open(&config.state_db)
            .with_context(|| format!("failed to open {}", config.state_db.display()))?;
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
            match discover_sealed_segments(&replay_dir) {
                Ok(paths) => {
                    tracing::info!(
                        path = %replay_dir.display(),
                        segments = paths.len(),
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

fn discover_sealed_segments(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if !directory.exists() {
        return Ok(paths);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_file() && is_sealed_notepack(&path) && !has_compressed_replacement(&path) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
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

        let found = discover_sealed_segments(directory.path()).unwrap();
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
