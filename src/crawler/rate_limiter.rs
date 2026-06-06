use anyhow::Result;
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

#[derive(Clone)]
pub struct GhostClient {
    builder: ghostwire::GhostwireBuilder,
    pub cookie: Option<String>,
}

impl GhostClient {
    pub fn new(builder: ghostwire::GhostwireBuilder, cookie: Option<String>) -> Self {
        Self { builder, cookie }
    }

    pub fn to_ghostwire(&self) -> Result<ghostwire::Ghostwire> {
        self.builder.clone().build().map_err(|e| anyhow::anyhow!(e))
    }
}

/// Build a Ghostwire client wrapper configured in stealth mode.
pub fn build_client(user_agent: &str, cookie: Option<&str>) -> Result<GhostClient> {
    let user_agent_opts = ghostwire::UserAgentOptions {
        custom: Some(user_agent.to_string()),
        ..Default::default()
    };

    let builder = ghostwire::Ghostwire::builder()
        .user_agent_opts(user_agent_opts)
        .min_request_interval_secs(0.0) // Respect rate limiting from RateLimiter
        .stealth(ghostwire::StealthConfig {
            enabled: true,
            human_like_delays: false, // Respect rate limiting from RateLimiter
            randomize_headers: true,
            browser_quirks: true,
            min_delay_secs: 0.0,
            max_delay_secs: 0.0,
        });

    Ok(GhostClient::new(builder, cookie.map(|s| s.to_string())))
}
