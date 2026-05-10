use anyhow::Result;
use reqwest::Client;
use std::time::Duration;
use tokio::time::sleep;

/// Controls the delay between outbound requests to avoid hammering the server.
#[derive(Clone)]
pub struct RateLimiter {
    delay_ms: u64,
}

impl RateLimiter {
    pub fn new(delay_ms: u64) -> Self {
        Self { delay_ms }
    }

    /// Sleep for the configured inter-request delay.
    pub async fn wait(&self) {
        sleep(Duration::from_millis(self.delay_ms)).await;
    }
}

/// Build a reqwest Client with the configured User-Agent and sensible timeouts.
pub fn build_client(user_agent: &str) -> Result<Client> {
    let client = Client::builder()
        .user_agent(user_agent)
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(60))
        .build()?;
    Ok(client)
}
