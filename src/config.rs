use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level application configuration (mirrors config.toml structure).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub concurrency: ConcurrencyConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub sync: SyncConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub base_url: String,
    pub user_agent: String,
    #[serde(default)]
    pub cookie: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            base_url: "https://server.elscione.com/".to_owned(),
            user_agent: format!(
                "elscione-sync/{} (personal mirror)",
                env!("CARGO_PKG_VERSION")
            ),
            cookie: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    pub dir: PathBuf,
}

impl Default for OutputConfig {
    fn default() -> Self {
        let home = dirs_home();
        Self {
            dir: home.join("elscione-mirror"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrencyConfig {
    /// Maximum number of simultaneous file downloads.
    pub max_parallel_downloads: usize,
    /// Maximum number of simultaneous directory crawl tasks.
    #[serde(default = "default_max_parallel_crawls")]
    pub max_parallel_crawls: usize,
    /// Milliseconds to wait between HTTP requests (crawl + download).
    pub delay_between_requests_ms: u64,
    /// Milliseconds to wait between directory crawl requests specifically.
    pub crawl_delay_ms: u64,
    /// Maximum retry attempts for directory crawling fetches.
    #[serde(default = "default_max_crawl_retries")]
    pub max_crawl_retries: usize,
}

fn default_max_parallel_crawls() -> usize {
    1
}

fn default_max_crawl_retries() -> usize {
    3
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_parallel_downloads: 2,
            max_parallel_crawls: 1,
            delay_between_requests_ms: 1500,
            crawl_delay_ms: 500,
            max_crawl_retries: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Initial back-off in seconds after a 429 response.
    pub backoff_initial_secs: u64,
    /// Maximum back-off cap in seconds.
    pub backoff_max_secs: u64,
    /// Exponential multiplier applied to back-off after each consecutive 429.
    pub backoff_multiplier: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            backoff_initial_secs: 60,
            backoff_max_secs: 900,
            backoff_multiplier: 2.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncConfig {
    /// Top-level folders to include. Empty = all folders.
    #[serde(default)]
    pub include_folders: Vec<String>,
    /// Glob-style patterns to exclude.
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    /// Only download files with these extensions (e.g. ["epub"]). Empty = all extensions.
    #[serde(default)]
    pub allowed_extensions: Vec<String>,
    /// Re-download a file when Last-Modified is unchanged but local size differs.
    #[serde(default = "default_true")]
    pub redownload_on_size_mismatch: bool,
}

fn default_true() -> bool {
    true
}

fn dirs_home() -> PathBuf {
    directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Returns the default config file path: `~/.config/elscione-sync/config.toml`.
pub fn default_config_path() -> PathBuf {
    ProjectDirs::from("com", "elscione", "elscione-sync")
        .map(|p| p.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("elscione-sync.toml"))
}

/// Returns the default state database path: `~/.local/share/elscione-sync/state.db`.
pub fn default_db_path() -> PathBuf {
    ProjectDirs::from("com", "elscione", "elscione-sync")
        .map(|p| p.data_dir().join("state.db"))
        .unwrap_or_else(|| PathBuf::from("elscione-sync.db"))
}

pub fn db_path_for_config(config_path: Option<&Path>) -> PathBuf {
    config_path
        .and_then(Path::parent)
        .map(|parent| parent.join("state.db"))
        .unwrap_or_else(default_db_path)
}

/// Load config from the given path (or the default path if `None`).
/// If no file exists, returns a default config and writes it to disk.
pub fn load(path: Option<&Path>) -> Result<Config> {
    let config_path = path.map(PathBuf::from).unwrap_or_else(default_config_path);

    if !config_path.exists() {
        let cfg = Config::default();
        save(&cfg, &config_path)
            .with_context(|| format!("writing default config to {}", config_path.display()))?;
        tracing::info!("Created default config at {}", config_path.display());
        return Ok(cfg);
    }

    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading config from {}", config_path.display()))?;
    let mut cfg: Config =
        toml::from_str(&raw).with_context(|| format!("parsing {}", config_path.display()))?;

    for ext in &mut cfg.sync.allowed_extensions {
        if ext.starts_with('.') {
            *ext = ext.trim_start_matches('.').to_owned();
        }
    }

    cfg.validate().context("validating configuration")?;
    tracing::debug!("Loaded config from {}", config_path.display());
    Ok(cfg)
}

/// Persist `config` to `path`, creating parent directories as needed.
pub fn save(config: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let toml = toml::to_string_pretty(config)?;
    std::fs::write(path, toml)?;
    Ok(())
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.concurrency.max_parallel_downloads < 1 {
            anyhow::bail!("concurrency.max_parallel_downloads must be >= 1");
        }
        if self.concurrency.max_parallel_crawls < 1 {
            anyhow::bail!("concurrency.max_parallel_crawls must be >= 1");
        }
        if self.rate_limit.backoff_multiplier < 1.0 {
            anyhow::bail!("rate_limit.backoff_multiplier must be >= 1.0");
        }
        if let Err(e) = reqwest::Url::parse(&self.server.base_url) {
            anyhow::bail!("server.base_url is not a valid URL: {e}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_config_uses_state_db_next_to_config_file() {
        let path = Path::new("/tmp/elscione/custom.toml");

        let db_path = db_path_for_config(Some(path));

        assert_eq!(db_path, PathBuf::from("/tmp/elscione/state.db"));
    }
}
