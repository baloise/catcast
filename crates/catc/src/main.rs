//! catc — CatCast CLI.
//!
//! Targeting model:
//! - no `--name`/`--all` -> every active stage in `catc.toml`
//! - `--name N` (repeatable) -> just those, regardless of active flag
//! - `--all` -> every registered stage, regardless of active flag

mod cfg;
mod cli;
mod completion;
mod default_logic;
mod edit;
mod net;
mod release;
mod status;

use anyhow::Result;
use clap::{CommandFactory, Parser};

#[tokio::main]
async fn main() -> Result<()> {
    // `COMPLETE=<shell> catc …` is a TAB press (or the shell asking for its
    // hook): answer and exit before anything else touches stdout.
    clap_complete::CompleteEnv::with_factory(cli::Cli::command).complete();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");
    let cli = cli::Cli::parse();
    cli::run(cli).await
}
