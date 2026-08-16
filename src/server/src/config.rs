use std::env;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub struct Settings {
    pub failure_threshold: u32,
    pub recovery_threshold: u32,
    pub getllm_base_url: String,
    pub getllm_api_key: String,
    pub getllm_route: Option<String>,
    pub request_timeout: Duration,
    pub text_enabled: bool,
    pub text_interval: Duration,
    pub text_model: String,
    pub text_prompt: String,
    pub text_max_tokens: u32,
    pub image_enabled: bool,
    pub image_interval: Duration,
    pub image_model: String,
    pub image_prompt: String,
    pub image_size: String,
    pub image_quality: Option<String>,
    pub image_response_format: Option<String>,
    pub image_timeout: Duration,
    pub feishu_app_id: Option<String>,
    pub feishu_app_secret: Option<String>,
    pub feishu_chat_id: Option<String>,
    pub feishu_timeout: Duration,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        let default_interval = optional_duration("ARGUS_INTERVAL_SECS")?;
        let text_enabled = boolean("GETLLM_TEXT_ENABLED", true)?;
        let image_enabled = boolean("GETLLM_IMAGE_ENABLED", true)?;
        if !text_enabled && !image_enabled {
            bail!("at least one GetLLM probe must be enabled");
        }

        Ok(Self {
            failure_threshold: positive_u32("ARGUS_FAILURE_THRESHOLD", 1)?,
            recovery_threshold: positive_u32("ARGUS_RECOVERY_THRESHOLD", 1)?,
            getllm_base_url: value("GETLLM_BASE_URL", "https://www.getllm.ai/v1"),
            getllm_api_key: required("GETLLM_API_KEY")?,
            getllm_route: optional("GETLLM_ROUTE"),
            request_timeout: duration("GETLLM_REQUEST_TIMEOUT_SECS", 30)?,
            text_enabled,
            text_interval: optional_duration("GETLLM_TEXT_INTERVAL_SECS")?
                .or(default_interval)
                .unwrap_or(Duration::from_secs(60)),
            text_model: value("GETLLM_TEXT_MODEL", "gpt-4o-mini"),
            text_prompt: value("GETLLM_TEXT_PROMPT", "ping"),
            text_max_tokens: positive_u32("GETLLM_TEXT_MAX_TOKENS", 1)?,
            image_enabled,
            image_interval: optional_duration("GETLLM_IMAGE_INTERVAL_SECS")?
                .or(default_interval)
                .unwrap_or(Duration::from_secs(600)),
            image_model: value("GETLLM_IMAGE_MODEL", "gpt-image-2"),
            image_prompt: value("GETLLM_IMAGE_PROMPT", "dot"),
            image_size: value("GETLLM_IMAGE_SIZE", "1024x1024"),
            image_quality: optional_or("GETLLM_IMAGE_QUALITY", Some("low")),
            image_response_format: optional_or("GETLLM_IMAGE_RESPONSE_FORMAT", None),
            image_timeout: duration("GETLLM_IMAGE_TIMEOUT_SECS", 180)?,
            feishu_app_id: optional("FEISHU_APP_ID"),
            feishu_app_secret: optional("FEISHU_APP_SECRET"),
            feishu_chat_id: optional("FEISHU_CHAT_ID"),
            feishu_timeout: duration("FEISHU_TIMEOUT_SECS", 10)?,
        })
    }
}

fn required(name: &str) -> Result<String> {
    optional(name)
        .with_context(|| format!("required environment variable {name} is missing or empty"))
}

fn optional(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn optional_or(name: &str, default: Option<&str>) -> Option<String> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(value.trim().to_owned()),
        Err(_) => default.map(ToOwned::to_owned),
    }
}

fn value(name: &str, default: &str) -> String {
    optional(name).unwrap_or_else(|| default.to_owned())
}

fn duration(name: &str, default: u64) -> Result<Duration> {
    let seconds = optional(name).map_or(Ok(default), |value| parse_u64(name, &value))?;
    if seconds == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(Duration::from_secs(seconds))
}

fn optional_duration(name: &str) -> Result<Option<Duration>> {
    optional(name)
        .map(|value| {
            let seconds = parse_u64(name, &value)?;
            if seconds == 0 {
                bail!("{name} must be greater than zero");
            }
            Ok(Duration::from_secs(seconds))
        })
        .transpose()
}

fn positive_u32(name: &str, default: u32) -> Result<u32> {
    let parsed = optional(name).map_or(Ok(default), |value| {
        value
            .parse::<u32>()
            .with_context(|| format!("{name} must be a positive integer"))
    })?;
    if parsed == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

fn boolean(name: &str, default: bool) -> Result<bool> {
    optional(name).map_or(Ok(default), |value| {
        match value.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => bail!("{name} must be true or false"),
        }
    })
}

fn parse_u64(name: &str, value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .with_context(|| format!("{name} must be a positive integer"))
}

#[cfg(test)]
mod tests {
    use super::parse_u64;

    #[test]
    fn parses_interval_seconds() {
        assert_eq!(parse_u64("INTERVAL", "60").ok(), Some(60));
        assert!(parse_u64("INTERVAL", "1m").is_err());
    }
}
