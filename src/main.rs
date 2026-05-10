mod cli;
mod config;
mod db;
mod crawler;
mod downloader;
mod tui;
mod sync;

use anyhow::Result;
use cli::{Cli, Commands};
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    let filter = if cli.verbose {
        EnvFilter::new("elscione_sync=debug,warn")
    } else {
        EnvFilter::new("elscione_sync=info,warn")
    };
    fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    // Load config
    let config = config::load(cli.config.as_deref())?;

    // Open DB
    let db = db::open(&config)?;

    match &cli.command {
        None | Some(Commands::Sync(_)) => {
            let opts = match &cli.command {
                Some(Commands::Sync(o)) => o.clone(),
                _ => Default::default(),
            };
            sync::run(config, db, opts).await?;
        }
        Some(Commands::Select) => {
            tui::run_folder_selector(&config, &db).await?;
        }
        Some(Commands::Status) => {
            sync::print_status(&db)?;
        }
        Some(Commands::Reset { errors_only }) => {
            sync::reset(&db, *errors_only)?;
        }
        Some(Commands::List { filter, status }) => {
            sync::list(&db, filter.as_deref(), status.as_deref())?;
        }
    }

    Ok(())
}
