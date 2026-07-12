//! Typed timeout policy. Each timeout has a different owner and fallback rule.

use std::time::Duration;

pub const DEFAULT_INIT_TIMEOUT_SECS: u64 = 30;
pub const CACHE_LIST_QUERY_TIMEOUT_SECS: u64 = 5;
pub const CACHE_REFRESH_TIMEOUT_SECS: u64 = 5;
pub const CACHE_PAGINATION_MAX_PAGES: usize = 100;
pub const NORMAL_IDLE_REAPER_SECS: u64 = 60;
pub const BROKER_IDLE_SECS: u64 = 60;
pub const MCP_TIMEOUT_ENV: &str = "MCP_TIMEOUT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutSource {
    UpstreamEnv,
    CurrentDefault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassThroughRequestTimeout {
    duration: Option<Duration>,
    source: TimeoutSource,
}

impl PassThroughRequestTimeout {
    pub fn from_env() -> Result<Self, String> {
        match std::env::var(MCP_TIMEOUT_ENV) {
            Ok(value) => Self::from_mcp_timeout_value(Some(&value)),
            Err(std::env::VarError::NotPresent) => Self::from_mcp_timeout_value(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(format!("{MCP_TIMEOUT_ENV} must be valid UTF-8"))
            }
        }
    }

    pub fn from_mcp_timeout_value(raw: Option<&str>) -> Result<Self, String> {
        let Some(raw) = raw else {
            return Ok(Self {
                duration: None,
                source: TimeoutSource::CurrentDefault,
            });
        };
        let millis = raw
            .parse::<u64>()
            .map_err(|_| format!("{MCP_TIMEOUT_ENV} must be a positive integer in milliseconds"))?;
        if millis == 0 {
            return Err(format!("{MCP_TIMEOUT_ENV} must be greater than 0"));
        }
        Ok(Self {
            duration: Some(Duration::from_millis(millis)),
            source: TimeoutSource::UpstreamEnv,
        })
    }

    pub fn duration(self) -> Option<Duration> {
        self.duration
    }

    #[allow(dead_code)]
    pub fn source(self) -> TimeoutSource {
        self.source
    }
}

pub fn default_init_timeout() -> Duration {
    Duration::from_secs(DEFAULT_INIT_TIMEOUT_SECS)
}
