use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "elscione-sync",
    about = "Resumable file mirror for server.elscione.com",
    version
)]
pub struct Cli {
    /// Path to config file (default: platform config dir)
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Enable verbose / debug logging
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start or resume syncing (default)
    Sync(SyncOpts),
    /// Open interactive folder selector
    Select,
    /// Show sync progress summary
    Status,
    /// Reset interrupted/error files back to 'pending'
    Reset {
        /// Only reset files with status 'error' (default: also resets 'downloading')
        #[arg(long)]
        errors_only: bool,
    },
    /// List discovered files
    List {
        /// Filter by substring in path
        #[arg(long)]
        filter: Option<String>,
        /// Filter by status (pending/done/error/skipped/downloading)
        #[arg(long)]
        status: Option<String>,
    },
    /// Open the config.toml file in your default editor
    EditConfig,
}

#[derive(Args, Debug, Clone, Default)]
pub struct SyncOpts {
    /// Override output directory
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Max parallel downloads
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Delay between HTTP requests in milliseconds
    #[arg(long)]
    pub delay: Option<u64>,

    /// Include only these top-level folders (can be repeated)
    #[arg(long = "include")]
    pub include: Vec<String>,

    /// Exclude glob patterns (can be repeated)
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,

    /// Only download files with these extensions (can be repeated, e.g. --extension epub)
    #[arg(long = "extension")]
    pub extensions: Vec<String>,

    /// Crawl and plan without downloading any files
    #[arg(long)]
    pub dry_run: bool,

    /// Skip crawl phase — only download already-pending files
    #[arg(long)]
    pub resume: bool,
}
