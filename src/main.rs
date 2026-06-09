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
use std::path::PathBuf;
use tracing_subscriber::{EnvFilter, fmt};

fn command_needs_database(command: &Option<Commands>) -> bool {
    !matches!(command, Some(Commands::EditConfig))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_config_does_not_require_database() {
        assert!(!command_needs_database(&Some(Commands::EditConfig)));
    }

    #[test]
    fn sync_and_inspection_commands_require_database() {
        assert!(command_needs_database(&None));
        assert!(command_needs_database(&Some(Commands::Status)));
        assert!(command_needs_database(&Some(Commands::List {
            filter: None,
            status: None,
        })));
    }
}

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

    if !command_needs_database(&cli.command) {
        edit_config(cli.config.clone())?;
        return Ok(());
    }

    // Open DB
    let db_path = config::db_path_for_config(cli.config.as_deref());
    let db = db::open_at(&db_path)?;

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
            edit_config(cli.config.clone())?;
        }
    }

    Ok(())
}

fn edit_config(config_path: Option<PathBuf>) -> Result<()> {
    let path = config_path.unwrap_or_else(config::default_config_path);

    // Ensure config exists before trying to open it.
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

    Ok(())
}
