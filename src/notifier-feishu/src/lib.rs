use std::time::{Duration, SystemTime};

use argus_core::{AlertEvent, AlertKind};
use argus_runtime::{Notifier, NotifyError};
use async_trait::async_trait;
use base64::Engine;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use thiserror::Error;
use url::Url;

type HmacSha256 = Hmac<Sha256>;

pub struct FeishuNotifier {
    webhook_url: Url,
    signing_secret: Option<String>,
    client: Client,
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("invalid Feishu webhook URL: {0}")]
    InvalidWebhookUrl(#[from] url::ParseError),
    #[error("Feishu webhook URL must use HTTPS")]
    InsecureWebhookUrl,
    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[from] reqwest::Error),
}

#[derive(Debug, Error)]
enum SendError {
    #[error("system clock is before the Unix epoch")]
    InvalidSystemClock,
    #[error("could not create Feishu signature")]
    InvalidSigningKey,
    #[error("Feishu webhook request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("Feishu webhook returned HTTP {status}: {body}")]
    Http {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("Feishu webhook rejected the message: code={code}, message={message}")]
    Rejected { code: i64, message: String },
    #[error("Feishu webhook returned an invalid response: {0}")]
    InvalidResponse(#[from] serde_json::Error),
}

#[derive(Deserialize)]
struct FeishuResponse {
    #[serde(default, alias = "StatusCode")]
    code: i64,
    #[serde(default, alias = "StatusMessage")]
    msg: String,
}

impl FeishuNotifier {
    pub fn new(
        webhook_url: &str,
        signing_secret: Option<String>,
        timeout: Duration,
    ) -> Result<Self, BuildError> {
        let webhook_url = Url::parse(webhook_url)?;
        if webhook_url.scheme() != "https" {
            return Err(BuildError::InsecureWebhookUrl);
        }
        let client = Client::builder().timeout(timeout).build()?;
        Ok(Self {
            webhook_url,
            signing_secret: signing_secret.filter(|secret| !secret.trim().is_empty()),
            client,
        })
    }

    async fn send(&self, event: &AlertEvent) -> Result<(), SendError> {
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| SendError::InvalidSystemClock)?
            .as_secs();
        let signature = self
            .signing_secret
            .as_deref()
            .map(|secret| sign(timestamp, secret))
            .transpose()?;
        let payload = payload(event, timestamp, signature.as_deref());
        let response = self
            .client
            .post(self.webhook_url.clone())
            .json(&payload)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(SendError::Http {
                status,
                body: truncate(&body, 512),
            });
        }
        let result: FeishuResponse = serde_json::from_str(&body)?;
        if result.code != 0 {
            return Err(SendError::Rejected {
                code: result.code,
                message: result.msg,
            });
        }
        Ok(())
    }
}

#[async_trait]
impl Notifier for FeishuNotifier {
    fn name(&self) -> &str {
        "feishu"
    }

    async fn notify(&self, event: &AlertEvent) -> Result<(), NotifyError> {
        self.send(event)
            .await
            .map_err(|error| Box::new(error) as NotifyError)
    }
}

fn sign(timestamp: u64, secret: &str) -> Result<String, SendError> {
    let string_to_sign = format!("{timestamp}\n{secret}");
    let signer = HmacSha256::new_from_slice(string_to_sign.as_bytes())
        .map_err(|_| SendError::InvalidSigningKey)?;
    let signature = signer.finalize().into_bytes();
    Ok(base64::engine::general_purpose::STANDARD.encode(signature))
}

fn payload(event: &AlertEvent, timestamp: u64, signature: Option<&str>) -> Value {
    let (icon, title) = match event.kind {
        AlertKind::Firing => ("🔴", "Argus 服务告警"),
        AlertKind::Recovered => ("🟢", "Argus 服务恢复"),
    };
    let mut lines = vec![
        format!("{icon} {title}"),
        format!("监控项：{}", event.monitor),
        format!("状态：{}", event.summary),
    ];
    match event.kind {
        AlertKind::Firing => lines.push(format!("连续失败：{} 次", event.consecutive_failures)),
        AlertKind::Recovered => {
            if let Some(duration) = event.incident_duration {
                lines.push(format!("故障时长：{}", format_duration(duration)));
            }
        }
    }

    let mut body = json!({
        "msg_type": "text",
        "content": {"text": lines.join("\n")},
    });
    if let Some(signature) = signature {
        body["timestamp"] = json!(timestamp.to_string());
        body["sign"] = json!(signature);
    }
    body
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}小时{minutes}分{seconds}秒")
    } else if minutes > 0 {
        format!("{minutes}分{seconds}秒")
    } else {
        format!("{seconds}秒")
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use argus_core::{AlertEvent, AlertKind};

    use super::{format_duration, payload, sign};

    #[test]
    fn signed_payload_contains_required_fields() {
        let event = AlertEvent {
            monitor: "getllm.text".to_owned(),
            kind: AlertKind::Firing,
            summary: "timeout".to_owned(),
            consecutive_failures: 2,
            incident_duration: None,
        };
        let signature = sign(1_599_360_473, "demo");
        assert!(signature.is_ok());
        let body = payload(&event, 1_599_360_473, signature.as_deref().ok());

        assert_eq!(body["msg_type"], "text");
        assert_eq!(body["timestamp"], "1599360473");
        assert!(body["sign"].as_str().is_some_and(|value| !value.is_empty()));
        assert!(
            body.pointer("/content/text")
                .and_then(|value| value.as_str())
                .is_some_and(|value| value.contains("连续失败：2 次"))
        );
    }

    #[test]
    fn formats_incident_duration() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5秒");
        assert_eq!(format_duration(Duration::from_secs(65)), "1分5秒");
        assert_eq!(format_duration(Duration::from_secs(3_665)), "1小时1分5秒");
    }
}
