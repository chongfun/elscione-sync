mod cli;
mod config;
mod db;
mod crawler;
mod downloader;
mod tui;
mod sync;

use anyhow::{Context, Result};
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

    // Create a cancellation token for graceful shutdown
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let ct = cancel_token.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::warn!("Ctrl-C received! Initiating graceful shutdown...");
            ct.cancel();
        }
    });

    match &cli.command {
        None | Some(Commands::Sync(_)) => {
            let opts = match &cli.command {
                Some(Commands::Sync(o)) => o.clone(),
                _ => Default::default(),
            };
            sync::run(config, db, opts, cancel_token).await?;
        }
        Some(Commands::Select) => {
            tui::run_folder_selector(&config, &db).await?;
        }
        Some(Commands::Status) => {
            sync::print_status(&db).await?;
        }
        Some(Commands::Reset { errors_only }) => {
            sync::reset(&db, *errors_only).await?;
        }
        Some(Commands::List { filter, status }) => {
            sync::list(&db, filter.as_deref(), status.as_deref()).await?;
        }
        Some(Commands::EditConfig) => {
            let path = cli.config.clone().unwrap_or_else(config::default_config_path);
            
            // Ensure config exists before trying to open it
            if !path.exists() {
                let _ = config::load(Some(&path))?;
            }
            
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| {
                if cfg!(target_os = "macos") {
                    "open".to_string()
                } else if cfg!(target_os = "windows") {
                    "notepad".to_string()
                } else {
                    "vi".to_string()
                }
            });

            println!("Opening {} in {}...", path.display(), editor);

            if editor == "open" {
                std::process::Command::new("open")
                    .arg("-t")
                    .arg(&path)
                    .status()
                    .with_context(|| format!("Failed to open editor: open -t {}", path.display()))?;
            } else {
                std::process::Command::new(&editor)
                    .arg(&path)
                    .status()
                    .with_context(|| format!("Failed to open editor: {} {}", editor, path.display()))?;
            }
        }
    }

    Ok(())
}
