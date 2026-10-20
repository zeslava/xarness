mod adapters;
mod agent;
mod config;
mod domain;
mod repomap;
mod skills;
mod tools;
mod tui;

use std::sync::Arc;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let config = config::Config::load()?;
    let root = std::env::current_dir()?.canonicalize()?;
    let skills = Arc::new(skills::Skills::discover(&root, &config.skills));
    tui::run(
        config,
        tools::ToolRegistry::new(root, Arc::clone(&skills)),
        skills,
    )
    .await
}
