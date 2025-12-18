//! Build script for pensieve-core.
//!
//! Compiles the nostr.proto file into Rust types using prost.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile the protobuf definitions
    prost_build::compile_protos(&["../../docs/nostr.proto"], &["../../docs/"])?;

    // Re-run if the proto file changes
    println!("cargo:rerun-if-changed=../../docs/nostr.proto");

    Ok(())
}
