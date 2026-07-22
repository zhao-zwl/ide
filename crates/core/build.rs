//! Build script: compile `proto/ide_core.proto` into Rust via tonic-build.
//!
//! Runs from the crate directory (`crates/core`), so the proto lives at
//! `../../proto/ide_core.proto` relative to here. The generated module is
//! included by `src/lib.rs` through `tonic::include_proto!("ide_core")`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(&["../../proto/ide_core.proto"], &["../../proto"])?;
    Ok(())
}
