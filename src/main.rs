mod adapters;
mod agent;
mod config;
mod domain;
mod metrics;
mod repomap;
mod session;
mod skills;
mod subagents;
mod tools;
mod tui;

use std::sync::Arc;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let config = config::Config::load()?;
    let root = std::env::current_dir()?.canonicalize()?;
    let skills = Arc::new(skills::Skills::discover(&root, &config.skills));
    let resume = std::env::args()
        .skip(1)
        .any(|a| a == "-c" || a == "--continue");
    tui::run(
        config,
        tools::ToolRegistry::new(root, Arc::clone(&skills)),
        skills,
        resume,
    )
    .await
}
