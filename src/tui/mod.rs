pub mod folder_selector;

use crate::config::Config;
use crate::crawler::session::ElscioneSession;
use crate::db::Db;
use anyhow::Result;

/// Launch the interactive folder selector TUI and persist the result to the DB.
pub async fn run_folder_selector(config: &Config, session: &ElscioneSession, db: &Db) -> Result<()> {
    folder_selector::run(config, session, db).await
}
