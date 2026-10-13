mod adapters;
mod agent;
mod config;
mod domain;
mod tools;
mod tui;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let config = config::Config::load()?;
    let root = std::env::current_dir()?.canonicalize()?;
    tui::run(config, tools::ToolRegistry::new(root)).await
}
