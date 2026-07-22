//! `aidea` binary: parse CLI args and dispatch to the shared Core (T09).

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    aidea::run().await
}
