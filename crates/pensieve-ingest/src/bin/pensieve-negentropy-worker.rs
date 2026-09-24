//! One job per process. No archive, dedupe, ledger or production secrets access.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
struct Args {
    /// Parent-owned Unix socket; never a TCP listener.
    #[arg(long)]
    socket: PathBuf,
    /// Required numeric UID of the parent Unix peer.
    #[arg(long)]
    parent_uid: u32,
}

fn main() {
    let args = Args::parse();
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Do not enable SDK debug logs: relay payloads/errors may contain private data.
    tracing_subscriber::fmt()
        .with_env_filter("off,pensieve_ingest::sync::worker=info,pensieve_negentropy_worker=info")
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("worker runtime");
    #[cfg(unix)]
    let outcome = runtime.block_on(pensieve_ingest::sync::worker::run(
        &args.socket,
        args.parent_uid,
    ));
    #[cfg(not(unix))]
    let outcome: Result<(), &str> = Err("worker requires Unix");
    let code = match outcome {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!(error = %error, "isolated worker failed; parent retains job");
            #[cfg(unix)]
            {
                error.exit_code()
            }
            #[cfg(not(unix))]
            1
        }
    };
    // Do not await possibly stuck SDK background task destruction. This process
    // owns no durable state; exit closes all sockets. Systemd is the hard backstop
    // when even the executor cannot make progress or the SDK exhausts memory.
    std::process::exit(code);
}
