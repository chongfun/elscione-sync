pub mod folder_selector;

use anyhow::Result;
use crate::config::Config;
use crate::db::Db;

/// Launch the interactive folder selector TUI and persist the result to the DB.
pub async fn run_folder_selector(config: &Config, db: &Db) -> Result<()> {
    folder_selector::run(config, db).await
}
