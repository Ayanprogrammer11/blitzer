mod app;
mod cli;
mod config;
mod download;
mod format;
mod terminal;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    if cli::run_from_args().await? {
        return Ok(());
    }
    terminal::run().await
}
