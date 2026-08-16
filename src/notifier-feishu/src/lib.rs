use std::time::{Duration, Instant};

use argus_core::{AlertEvent, AlertKind};
use argus_runtime::{Notifier, NotifyError};
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;

const FEISHU_BASE_URL: &str = "https://open.feishu.cn";
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;
const ERROR_DETAIL_LIMIT: usize = 512;

pub struct FeishuNotifier {
    app_id: String,
    app_secret: String,
    chat_id: String,
    token_endpoint: Url,
    message_endpoint: Url,
    client: Client,
    token_cache: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    value: String,
    refresh_at: Instant,
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("invalid Feishu API base URL: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),
    #[error("Feishu App ID cannot be empty")]
    EmptyAppId,
    #[error("Feishu App Secret cannot be empty")]
    EmptyAppSecret,
    #[error("Feishu chat ID cannot be empty")]
    EmptyChatId,
    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[from] reqwest::Error),
}

#[derive(Debug, Error)]
enum SendError {
    #[error("Feishu API request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("Feishu token API returned HTTP {status}: {detail}")]
    TokenHttp { status: StatusCode, detail: String },
    #[error("Feishu token API rejected the credentials: code={code}, message={message}")]
    TokenRejected { code: i64, message: String },
    #[error("Feishu token API returned no access token")]
    MissingToken,
    #[error("Feishu token lifetime is too large")]
    InvalidTokenLifetime,
    #[error("Feishu message API returned HTTP {status}: {detail}")]
    MessageHttp { status: StatusCode, detail: String },
    #[error("Feishu message API rejected the message: code={code}, message={message}")]
    MessageRejected { code: i64, message: String },
    #[error("Feishu API returned an invalid response: {0}")]
    InvalidResponse(#[from] serde_json::Error),
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    msg: String,
    tenant_access_token: Option<String>,
    #[serde(default)]
    expire: u64,
}

#[derive(Deserialize)]
struct FeishuResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    msg: String,
}

impl FeishuNotifier {
    pub fn new(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        chat_id: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, BuildError> {
        Self::build(
            FEISHU_BASE_URL,
            app_id.into(),
            app_secret.into(),
            chat_id.into(),
            timeout,
        )
    }

    fn build(
        base_url: &str,
        app_id: String,
        app_secret: String,
        chat_id: String,
        timeout: Duration,
    ) -> Result<Self, BuildError> {
        if app_id.trim().is_empty() {
            return Err(BuildError::EmptyAppId);
        }
        if app_secret.trim().is_empty() {
            return Err(BuildError::EmptyAppSecret);
        }
        if chat_id.trim().is_empty() {
            return Err(BuildError::EmptyChatId);
        }

        let mut normalized = base_url.trim_end_matches('/').to_owned();
        normalized.push('/');
        let base = Url::parse(&normalized)?;
        let token_endpoint = base.join("open-apis/auth/v3/tenant_access_token/internal/")?;
        let message_endpoint = base.join("open-apis/im/v1/messages?receive_id_type=chat_id")?;
        let client = Client::builder().timeout(timeout).build()?;
        Ok(Self {
            app_id,
            app_secret,
            chat_id,
            token_endpoint,
            message_endpoint,
            client,
            token_cache: Mutex::new(None),
        })
    }

    async fn send(&self, event: &AlertEvent) -> Result<(), SendError> {
        let token = self.access_token().await?;
        let content = serde_json::to_string(&json!({"text": message_text(event)}))?;
        let response = self
            .client
            .post(self.message_endpoint.clone())
            .bearer_auth(token)
            .json(&json!({
                "receive_id": self.chat_id,
                "msg_type": "text",
                "content": content,
            }))
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(SendError::MessageHttp {
                status,
                detail: response_detail(&body),
            });
        }
        let result: FeishuResponse = serde_json::from_str(&body)?;
        if result.code != 0 {
            return Err(SendError::MessageRejected {
                code: result.code,
                message: result.msg,
            });
        }
        Ok(())
    }

    async fn access_token(&self) -> Result<String, SendError> {
        let mut cache = self.token_cache.lock().await;
        if let Some(token) = cache.as_ref()
            && Instant::now() < token.refresh_at
        {
            return Ok(token.value.clone());
        }

        let response = self
            .client
            .post(self.token_endpoint.clone())
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(SendError::TokenHttp {
                status,
                detail: response_detail(&body),
            });
        }

        let result: TokenResponse = serde_json::from_str(&body)?;
        if result.code != 0 {
            return Err(SendError::TokenRejected {
                code: result.code,
                message: result.msg,
            });
        }
        let token = result
            .tenant_access_token
            .filter(|value| !value.trim().is_empty())
            .ok_or(SendError::MissingToken)?;
        let refresh_in = result
            .expire
            .saturating_sub(TOKEN_REFRESH_MARGIN_SECS)
            .max(1);
        let refresh_at = Instant::now()
            .checked_add(Duration::from_secs(refresh_in))
            .ok_or(SendError::InvalidTokenLifetime)?;
        *cache = Some(CachedToken {
            value: token.clone(),
            refresh_at,
        });
        Ok(token)
    }
}

#[async_trait]
impl Notifier for FeishuNotifier {
    fn name(&self) -> &str {
        "feishu.app"
    }

    async fn notify(&self, event: &AlertEvent) -> Result<(), NotifyError> {
        self.send(event)
            .await
            .map_err(|error| Box::new(error) as NotifyError)
    }
}

fn message_text(event: &AlertEvent) -> String {
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
    lines.join("\n")
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

fn response_detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("msg")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| truncate(body, ERROR_DETAIL_LIMIT))
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
    use std::error::Error;
    use std::io;
    use std::time::Duration;

    use argus_core::{AlertEvent, AlertKind};
    use argus_runtime::Notifier;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::{FeishuNotifier, format_duration, message_text};

    #[test]
    fn formats_alert_text() {
        let event = firing_event();
        let text = message_text(&event);

        assert!(text.contains("Argus 服务告警"));
        assert!(text.contains("监控项：getllm.text"));
        assert!(text.contains("连续失败：2 次"));
    }

    #[test]
    fn formats_incident_duration() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5秒");
        assert_eq!(format_duration(Duration::from_secs(65)), "1分5秒");
        assert_eq!(format_duration(Duration::from_secs(3_665)), "1小时1分5秒");
    }

    #[tokio::test]
    async fn obtains_and_reuses_token_to_send_messages() -> Result<(), Box<dyn Error + Send + Sync>>
    {
        let (base_url, requests, server) = mock_server(vec![
            (
                "200 OK",
                r#"{"code":0,"msg":"ok","tenant_access_token":"test-token","expire":7200}"#,
            ),
            ("200 OK", r#"{"code":0,"msg":"success","data":{}}"#),
            ("200 OK", r#"{"code":0,"msg":"success","data":{}}"#),
        ])
        .await?;
        let notifier = FeishuNotifier::build(
            &base_url,
            "cli_test".to_owned(),
            "secret".to_owned(),
            "oc_test".to_owned(),
            Duration::from_secs(2),
        )?;

        notifier.notify(&firing_event()).await?;
        notifier.notify(&firing_event()).await?;
        let requests = requests.await?;
        server.await??;
        let token_request = request_at(&requests, 0)?;
        let first_message = request_at(&requests, 1)?;
        let second_message = request_at(&requests, 2)?;

        assert!(
            token_request
                .starts_with("POST /open-apis/auth/v3/tenant_access_token/internal/ HTTP/1.1")
        );
        assert!(token_request.contains("\"app_id\":\"cli_test\""));
        assert!(
            first_message
                .starts_with("POST /open-apis/im/v1/messages?receive_id_type=chat_id HTTP/1.1")
        );
        assert!(
            first_message
                .to_ascii_lowercase()
                .contains("authorization: bearer test-token")
        );
        assert!(first_message.contains("\"receive_id\":\"oc_test\""));
        assert!(first_message.contains("\"msg_type\":\"text\""));
        assert!(
            second_message
                .to_ascii_lowercase()
                .contains("authorization: bearer test-token")
        );
        assert_eq!(requests.len(), 3);
        Ok(())
    }

    fn firing_event() -> AlertEvent {
        AlertEvent {
            monitor: "getllm.text".to_owned(),
            kind: AlertKind::Firing,
            summary: "timeout".to_owned(),
            consecutive_failures: 2,
            incident_duration: None,
        }
    }

    async fn mock_server(
        responses: Vec<(&'static str, &'static str)>,
    ) -> io::Result<(
        String,
        oneshot::Receiver<Vec<String>>,
        JoinHandle<io::Result<()>>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (request_tx, request_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await?;
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let read = stream.read(&mut chunk).await?;
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request_is_complete(&request) {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request).into_owned());
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                stream.shutdown().await?;
            }
            let _ = request_tx.send(requests);
            Ok(())
        });
        Ok((format!("http://{address}"), request_rx, server))
    }

    fn request_at(requests: &[String], index: usize) -> io::Result<&str> {
        requests.get(index).map(String::as_str).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("mock server received no request at index {index}"),
            )
        })
    }

    fn request_is_complete(request: &[u8]) -> bool {
        let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        });
        content_length.is_none_or(|length| request.len() >= header_end + length)
    }
}
