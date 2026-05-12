//! catc — CatCast CLI.
//!
//! Targeting model:
//! - no `--name`/`--all` -> every active stage in `catc.toml`
//! - `--name N` (repeatable) -> just those, regardless of active flag
//! - `--all` -> every registered stage, regardless of active flag

mod cfg;
mod cli;
mod net;

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");
    let cli = cli::Cli::parse();
    cli::run(cli).await
}
