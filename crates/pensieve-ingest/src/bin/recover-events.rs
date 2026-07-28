//! Resumably recover exact Nostr event IDs from an operator-supplied relay set.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures_util::{StreamExt, stream};
use nostr_sdk::prelude::{Client, Event, EventId, Filter, JsonUtil};

#[derive(Debug, Parser)]
#[command(about = "Resumably recover exact Nostr event IDs from public relays")]
struct Args {
    /// One 64-character event ID per line.
    ids: PathBuf,
    /// Append-only validated recovered-event JSONL.
    output_jsonl: PathBuf,
    /// Atomically replaced snapshot of IDs not present in the output.
    missing_ids: PathBuf,
    /// Relay URLs, one per line; blank lines and # comments are ignored.
    #[arg(long)]
    relay_file: PathBuf,
    /// Append-only operator journal for batch outcomes.
    #[arg(long)]
    journal: Option<PathBuf>,
    /// Progressively smaller exact-ID request sizes.
    #[arg(long, value_delimiter = ',', default_value = "100,20,1")]
    batch_sizes: Vec<usize>,
    /// Parallel relay-pool requests.
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    /// Timeout for one batch request.
    #[arg(long, default_value_t = 12)]
    request_timeout_secs: u64,
    /// Relay connection warm-up before requests begin.
    #[arg(long, default_value_t = 5)]
    connect_wait_secs: u64,
}

fn main() {
    // The workspace enables both rustls providers transitively. Relay connections
    // cannot choose safely between them without a process-level default.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    if let Err(error) = run() {
        eprintln!("event recovery failed: {error:#}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run() -> Result<()> {
    let args = Args::parse();
    if args.concurrency == 0 {
        bail!("--concurrency must be greater than zero");
    }
    if args.batch_sizes.is_empty() || args.batch_sizes.contains(&0) {
        bail!("--batch-sizes must contain only positive values");
    }
    let ids = read_ids(&args.ids)?;
    let target_ids: HashSet<_> = ids.iter().copied().collect();
    let mut recovered = read_recovered(&args.output_jsonl, &target_ids)?;
    let relays = read_relays(&args.relay_file)?;
    eprintln!(
        "targets={} already_recovered={} relays={}",
        ids.len(),
        recovered.len(),
        relays.len()
    );

    let mut clients = Vec::with_capacity(args.concurrency);
    for worker in 0..args.concurrency {
        let client = Client::default();
        let mut added = 0usize;
        for relay in &relays {
            match client.add_relay(relay).await {
                Ok(true) => added += 1,
                Ok(false) => {}
                Err(error) => {
                    eprintln!("worker={worker} failed_relay={relay} error={error}");
                }
            }
        }
        if added == 0 {
            bail!("worker {worker} could not add any configured relay");
        }
        client.connect().await;
        clients.push(client);
    }
    tokio::time::sleep(Duration::from_secs(args.connect_wait_secs)).await;

    let output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.output_jsonl)
        .with_context(|| format!("failed to open {}", args.output_jsonl.display()))?;
    let mut writer = BufWriter::new(output);
    let mut journal = args
        .journal
        .as_ref()
        .map(|path| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("failed to open {}", path.display()))
                .map(BufWriter::new)
        })
        .transpose()?;

    for (pass_index, batch_size) in args.batch_sizes.iter().copied().enumerate() {
        let missing: Vec<_> = ids
            .iter()
            .copied()
            .filter(|id| !recovered.contains(id))
            .collect();
        if missing.is_empty() {
            break;
        }
        let batches: Vec<_> = missing.chunks(batch_size).map(<[_]>::to_vec).collect();
        let batch_count = batches.len();
        eprintln!(
            "pass={} batch_size={} missing={} batches={}",
            pass_index + 1,
            batch_size,
            missing.len(),
            batch_count
        );

        let requests = stream::iter(batches.into_iter().enumerate().map(|(batch_index, batch)| {
            let client = clients[batch_index % clients.len()].clone();
            let timeout = Duration::from_secs(args.request_timeout_secs);
            async move {
                let filter = Filter::new().ids(batch);
                (batch_index, client.fetch_events(filter, timeout).await)
            }
        }))
        .buffer_unordered(args.concurrency);
        tokio::pin!(requests);
        let mut completed = 0usize;
        while let Some((batch_index, result)) = requests.next().await {
            let mut added = 0usize;
            match result {
                Ok(events) => {
                    for event in events {
                        if !target_ids.contains(&event.id) || recovered.contains(&event.id) {
                            continue;
                        }
                        event.verify().with_context(|| {
                            format!("relay returned invalid event {}", event.id)
                        })?;
                        writeln!(writer, "{}", event.as_json())?;
                        recovered.insert(event.id);
                        added += 1;
                    }
                    if added > 0 {
                        writer.flush()?;
                        writer.get_ref().sync_data()?;
                    }
                    write_journal(
                        journal.as_mut(),
                        pass_index + 1,
                        batch_index + 1,
                        "ok",
                        added,
                        None,
                    )?;
                }
                Err(error) => {
                    eprintln!(
                        "fetch_error pass={} batch={} error={error}",
                        pass_index + 1,
                        batch_index + 1
                    );
                    write_journal(
                        journal.as_mut(),
                        pass_index + 1,
                        batch_index + 1,
                        "error",
                        0,
                        Some(&error.to_string()),
                    )?;
                }
            }
            completed += 1;
            if completed.is_multiple_of(10) || completed == batch_count {
                eprintln!(
                    "progress pass={} batches={}/{} recovered={}/{}",
                    pass_index + 1,
                    completed,
                    batch_count,
                    recovered.len(),
                    ids.len()
                );
            }
        }
    }
    writer.flush()?;
    writer.get_ref().sync_data()?;
    if let Some(journal) = journal.as_mut() {
        journal.flush()?;
        journal.get_ref().sync_data()?;
    }
    let missing = write_missing_atomically(&args.missing_ids, &ids, &recovered)?;
    eprintln!(
        "complete targets={} recovered={} missing={}",
        ids.len(),
        recovered.len(),
        missing
    );
    Ok(())
}

fn read_ids(path: &Path) -> Result<Vec<EventId>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut ids = Vec::new();
    let mut unique = HashSet::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let value = line.with_context(|| format!("failed to read line {}", index + 1))?;
        let id = EventId::from_hex(&value)
            .with_context(|| format!("invalid event ID on line {}", index + 1))?;
        if !unique.insert(id) {
            bail!("duplicate event ID on line {}", index + 1);
        }
        ids.push(id);
    }
    if ids.is_empty() {
        bail!("target ID file is empty");
    }
    Ok(ids)
}

fn read_relays(path: &Path) -> Result<Vec<String>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut relays = Vec::new();
    let mut unique = HashSet::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let value = line.split('#').next().unwrap_or_default().trim();
        if !value.is_empty() && unique.insert(value.to_owned()) {
            relays.push(value.to_owned());
        }
    }
    if relays.is_empty() {
        bail!("relay file has no relay URLs");
    }
    Ok(relays)
}

fn read_recovered(path: &Path, target_ids: &HashSet<EventId>) -> Result<HashSet<EventId>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut recovered = HashSet::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let json = line.with_context(|| format!("failed to read output line {}", index + 1))?;
        let event = Event::from_json(json)
            .with_context(|| format!("invalid output event on line {}", index + 1))?;
        event
            .verify()
            .with_context(|| format!("invalid output signature on line {}", index + 1))?;
        if !target_ids.contains(&event.id) {
            bail!("output contains non-target event {}", event.id);
        }
        if !recovered.insert(event.id) {
            bail!("output contains duplicate event {}", event.id);
        }
    }
    Ok(recovered)
}

fn write_missing_atomically(
    path: &Path,
    ids: &[EventId],
    recovered: &HashSet<EventId>,
) -> Result<usize> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut missing = 0usize;
    for id in ids {
        if !recovered.contains(id) {
            writeln!(temporary, "{id}")?;
            missing += 1;
        }
    }
    temporary.flush()?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(missing)
}

fn write_journal(
    journal: Option<&mut BufWriter<File>>,
    pass: usize,
    batch: usize,
    result: &str,
    recovered: usize,
    error: Option<&str>,
) -> Result<()> {
    let Some(journal) = journal else {
        return Ok(());
    };
    let entry = serde_json::json!({
        "pass": pass,
        "batch": batch,
        "result": result,
        "recovered": recovered,
        "error": error,
    });
    writeln!(journal, "{entry}")?;
    journal.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::{EventBuilder, Keys, Kind};

    use super::*;

    fn event(content: &str) -> Event {
        let keys = Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
            .expect("keys");
        EventBuilder::new(Kind::TextNote, content)
            .sign_with_keys(&keys)
            .expect("event")
    }

    #[test]
    fn duplicate_target_ids_are_rejected() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("ids.hex");
        let id = event("one").id;
        fs::write(&path, format!("{id}\n{id}\n")).expect("write IDs");
        assert!(
            read_ids(&path)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
    }

    #[test]
    fn recovered_output_must_be_unique_and_in_target_set() {
        let directory = tempfile::tempdir().expect("directory");
        let output = directory.path().join("events.jsonl");
        let target = event("target");
        let other = event("other");
        fs::write(&output, format!("{}\n", other.as_json())).expect("output");
        let error = read_recovered(&output, &HashSet::from([target.id])).expect_err("non-target");
        assert!(error.to_string().contains("non-target"));
    }

    #[test]
    fn missing_snapshot_preserves_target_order_and_replaces_old_state() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("missing.hex");
        let first = event("first").id;
        let second = event("second").id;
        let third = event("third").id;
        fs::write(&path, "stale\n").expect("old snapshot");
        let missing =
            write_missing_atomically(&path, &[first, second, third], &HashSet::from([second]))
                .expect("missing snapshot");
        assert_eq!(missing, 2);
        assert_eq!(
            fs::read_to_string(path).expect("snapshot"),
            format!("{first}\n{third}\n")
        );
    }
}
