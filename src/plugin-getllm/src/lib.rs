use std::time::{Duration, Instant};

use argus_core::CheckOutcome;
use argus_runtime::MonitorPlugin;
use async_trait::async_trait;
use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

const RESPONSE_ERROR_LIMIT: usize = 512;
const IMAGE_SCAN_TAIL: usize = 512;

#[derive(Clone, Debug)]
pub struct TextProbeConfig {
    pub model: String,
    pub prompt: String,
    pub max_tokens: u32,
}

#[derive(Clone, Debug)]
pub struct ImageProbeConfig {
    pub model: String,
    pub prompt: String,
    pub size: String,
    pub quality: Option<String>,
    pub response_format: Option<String>,
}

#[derive(Clone, Debug)]
enum Probe {
    Text(TextProbeConfig),
    Image(ImageProbeConfig),
}

pub struct GetLlmMonitor {
    name: String,
    endpoint: Url,
    models_endpoint: Url,
    api_key: String,
    route: Option<String>,
    interval: Duration,
    client: Client,
    probe: Probe,
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("invalid GetLLM base URL: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),
    #[error("failed to build HTTP client: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("GetLLM API key cannot be empty")]
    EmptyApiKey,
}

impl GetLlmMonitor {
    pub fn text(
        base_url: &str,
        api_key: impl Into<String>,
        route: Option<String>,
        interval: Duration,
        timeout: Duration,
        config: TextProbeConfig,
    ) -> Result<Self, BuildError> {
        Self::build(
            "getllm.text",
            base_url,
            "chat/completions",
            api_key.into(),
            route,
            interval,
            timeout,
            Probe::Text(config),
        )
    }

    pub fn image(
        base_url: &str,
        api_key: impl Into<String>,
        route: Option<String>,
        interval: Duration,
        timeout: Duration,
        config: ImageProbeConfig,
    ) -> Result<Self, BuildError> {
        Self::build(
            "getllm.image",
            base_url,
            "images/generations",
            api_key.into(),
            route,
            interval,
            timeout,
            Probe::Image(config),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        name: &str,
        base_url: &str,
        path: &str,
        api_key: String,
        route: Option<String>,
        interval: Duration,
        timeout: Duration,
        probe: Probe,
    ) -> Result<Self, BuildError> {
        if api_key.trim().is_empty() {
            return Err(BuildError::EmptyApiKey);
        }
        let mut normalized = base_url.trim_end_matches('/').to_owned();
        normalized.push('/');
        let base = Url::parse(&normalized)?;
        let endpoint = base.join(path)?;
        let models_endpoint = base.join("models")?;
        let client = Client::builder().timeout(timeout).build()?;
        Ok(Self {
            name: name.to_owned(),
            endpoint,
            models_endpoint,
            api_key,
            route: route.filter(|value| !value.trim().is_empty()),
            interval: interval.max(Duration::from_secs(1)),
            client,
            probe,
        })
    }

    async fn run_text(&self, config: &TextProbeConfig, started: Instant) -> CheckOutcome {
        let payload = json!({
            "model": config.model,
            "messages": [{"role": "user", "content": config.prompt}],
            "max_tokens": config.max_tokens.max(1),
            "stream": false,
            "temperature": 0,
        });
        let response = match self.send(payload).await {
            Ok(response) => response,
            Err(error) => {
                return CheckOutcome::unhealthy(
                    format!("request failed: {error}"),
                    started.elapsed(),
                );
            }
        };
        let status = response.status();
        let request_id = response_header(&response, "x-request-id")
            .or_else(|| response_header(&response, "x-getllm-request-id"));
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                return CheckOutcome::unhealthy(
                    format!("could not read response: {error}"),
                    started.elapsed(),
                );
            }
        };
        if !status.is_success() {
            return http_failure(status, &body, started.elapsed());
        }
        let value = match serde_json::from_slice::<Value>(&body) {
            Ok(value) => value,
            Err(error) => {
                return CheckOutcome::unhealthy(
                    format!("invalid JSON response: {error}"),
                    started.elapsed(),
                );
            }
        };
        if value.pointer("/choices/0/message").is_none()
            && value.pointer("/choices/0/text").is_none()
        {
            return CheckOutcome::unhealthy(
                "HTTP 200 response contained no completion choice",
                started.elapsed(),
            );
        }

        CheckOutcome::healthy(
            success_summary(&config.model, request_id.as_deref()),
            started.elapsed(),
        )
    }

    async fn run_image(&self, config: &ImageProbeConfig, started: Instant) -> CheckOutcome {
        if let Err(outcome) = self
            .ensure_image_model_available(&config.model, started)
            .await
        {
            return outcome;
        }

        let mut payload = json!({
            "model": config.model,
            "prompt": config.prompt,
            "n": 1,
            "size": config.size,
        });
        if let Some(quality) = config.quality.as_deref() {
            payload["quality"] = json!(quality);
        }
        if let Some(response_format) = config.response_format.as_deref() {
            payload["response_format"] = json!(response_format);
        }

        let mut response = match self.send(payload).await {
            Ok(response) => response,
            Err(error) => {
                return CheckOutcome::unhealthy(
                    format!("request failed: {error}"),
                    started.elapsed(),
                );
            }
        };
        let status = response.status();
        let request_id = response_header(&response, "x-request-id")
            .or_else(|| response_header(&response, "x-getllm-request-id"));
        if !status.is_success() {
            let body = match response.bytes().await {
                Ok(body) => body,
                Err(error) => {
                    return CheckOutcome::unhealthy(
                        format!("HTTP {status}; could not read error response: {error}"),
                        started.elapsed(),
                    );
                }
            };
            return http_failure(status, &body, started.elapsed());
        }

        let mut tail = Vec::with_capacity(IMAGE_SCAN_TAIL * 2);
        let mut has_image = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    tail.extend_from_slice(&chunk);
                    if contains_nonempty_string_field(&tail, b"url")
                        || contains_nonempty_string_field(&tail, b"b64_json")
                    {
                        has_image = true;
                    }
                    if tail.len() > IMAGE_SCAN_TAIL {
                        let keep_from = tail.len() - IMAGE_SCAN_TAIL;
                        tail.drain(..keep_from);
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    return CheckOutcome::unhealthy(
                        format!("could not read image response: {error}"),
                        started.elapsed(),
                    );
                }
            }
        }

        if has_image {
            CheckOutcome::healthy(
                success_summary(&config.model, request_id.as_deref()),
                started.elapsed(),
            )
        } else {
            CheckOutcome::unhealthy(
                "HTTP 200 response contained no image URL or base64 payload",
                started.elapsed(),
            )
        }
    }

    async fn send(&self, payload: Value) -> Result<Response, reqwest::Error> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(&self.api_key)
            .json(&payload);
        if let Some(route) = self.route.as_deref() {
            request = request.header("X-GetLLM-Route", route);
        }
        request.send().await
    }

    async fn ensure_image_model_available(
        &self,
        model: &str,
        started: Instant,
    ) -> Result<(), CheckOutcome> {
        let response = match self
            .client
            .get(self.models_endpoint.clone())
            .bearer_auth(&self.api_key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return Err(CheckOutcome::unhealthy(
                    format!("model discovery request failed: {error}"),
                    started.elapsed(),
                ));
            }
        };
        let status = response.status();
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                return Err(CheckOutcome::unhealthy(
                    format!("could not read model discovery response: {error}"),
                    started.elapsed(),
                ));
            }
        };
        if !status.is_success() {
            let outcome = http_failure(status, &body, started.elapsed());
            return Err(CheckOutcome::unhealthy(
                format!("model discovery failed: {}", outcome.summary),
                outcome.latency,
            ));
        }

        let value = serde_json::from_slice::<Value>(&body).map_err(|error| {
            CheckOutcome::unhealthy(
                format!("invalid model discovery JSON: {error}"),
                started.elapsed(),
            )
        })?;
        let models = value.get("data").and_then(Value::as_array).ok_or_else(|| {
            CheckOutcome::unhealthy(
                "model discovery response contained no data array",
                started.elapsed(),
            )
        })?;
        let Some(discovered) = models
            .iter()
            .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(model))
        else {
            return Err(CheckOutcome::unhealthy(
                format!("image model {model} is not advertised by GET /v1/models"),
                started.elapsed(),
            ));
        };

        if let Some(output_modalities) = discovered
            .pointer("/hermes/output_modalities")
            .and_then(Value::as_array)
            && !output_modalities
                .iter()
                .any(|modality| modality.as_str() == Some("image"))
        {
            return Err(CheckOutcome::unhealthy(
                format!("model {model} is advertised without image output capability"),
                started.elapsed(),
            ));
        }

        Ok(())
    }
}

#[async_trait]
impl MonitorPlugin for GetLlmMonitor {
    fn name(&self) -> &str {
        &self.name
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    async fn check(&self) -> CheckOutcome {
        let started = Instant::now();
        match &self.probe {
            Probe::Text(config) => self.run_text(config, started).await,
            Probe::Image(config) => self.run_image(config, started).await,
        }
    }
}

fn response_header(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn success_summary(model: &str, request_id: Option<&str>) -> String {
    request_id.map_or_else(
        || format!("model={model}"),
        |request_id| format!("model={model}, request_id={request_id}"),
    )
}

fn http_failure(status: StatusCode, body: &[u8], latency: Duration) -> CheckOutcome {
    let detail = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    let detail = truncate(&detail, RESPONSE_ERROR_LIMIT);
    CheckOutcome::unhealthy(format!("HTTP {status}: {detail}"), latency)
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

fn contains_nonempty_string_field(bytes: &[u8], field: &[u8]) -> bool {
    let mut needle = Vec::with_capacity(field.len() + 2);
    needle.push(b'"');
    needle.extend_from_slice(field);
    needle.push(b'"');

    let mut offset = 0;
    while offset + needle.len() <= bytes.len() {
        if !bytes[offset..].starts_with(&needle) {
            offset += 1;
            continue;
        }
        let mut rest = &bytes[offset + needle.len()..];
        rest = trim_ascii_start(rest);
        let Some(after_colon) = rest.strip_prefix(b":") else {
            offset += 1;
            continue;
        };
        rest = trim_ascii_start(after_colon);
        if rest.first() == Some(&b'"') && rest.get(1).is_some_and(|byte| *byte != b'"') {
            return true;
        }
        offset += 1;
    }
    false
}

fn trim_ascii_start(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;
    use std::time::Duration;

    use argus_core::Health;
    use argus_runtime::MonitorPlugin;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::{
        GetLlmMonitor, ImageProbeConfig, TextProbeConfig, contains_nonempty_string_field, truncate,
    };

    #[test]
    fn identifies_only_nonempty_image_fields() {
        assert!(contains_nonempty_string_field(
            br#"{"data":[{"url":"https://example.test/image.png"}]}"#,
            b"url"
        ));
        assert!(contains_nonempty_string_field(
            br#"{"b64_json" : "abc"}"#,
            b"b64_json"
        ));
        assert!(!contains_nonempty_string_field(
            br#"{"data":[{"url":""}]}"#,
            b"url"
        ));
        assert!(!contains_nonempty_string_field(
            br#"{"error":{"message":"url"}}"#,
            b"url"
        ));
    }

    #[test]
    fn truncates_on_character_boundaries() {
        assert_eq!(truncate("正常响应", 2), "正常…");
        assert_eq!(truncate("ok", 2), "ok");
    }

    #[tokio::test]
    async fn text_probe_validates_an_openai_completion() -> Result<(), Box<dyn Error>> {
        let (base_url, requests, server) = mock_server(vec![(
            "200 OK",
            r#"{"choices":[{"message":{"role":"assistant","content":"pong"}}]}"#,
        )])
        .await?;
        let monitor = GetLlmMonitor::text(
            &base_url,
            "test-key",
            Some("cheapest".to_owned()),
            Duration::from_secs(60),
            Duration::from_secs(2),
            TextProbeConfig {
                model: "test-text".to_owned(),
                prompt: "ping".to_owned(),
                max_tokens: 1,
            },
        )?;

        let outcome = monitor.check().await;
        let requests = requests.await?;
        server.await??;
        let request = request_at(&requests, 0)?;

        assert_eq!(outcome.health, Health::Healthy);
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-key")
        );
        assert!(request.contains("\"max_tokens\":1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-getllm-route: cheapest")
        );
        Ok(())
    }

    #[tokio::test]
    async fn image_probe_requires_an_image_field() -> Result<(), Box<dyn Error>> {
        let (base_url, requests, server) = mock_server(vec![
            (
                "200 OK",
                r#"{"object":"list","data":[{"id":"test-image","hermes":{"output_modalities":["image"]}}]}"#,
            ),
            (
                "200 OK",
                r#"{"created":1,"data":[{"url":"https://example.test/image.png"}]}"#,
            ),
        ])
        .await?;
        let monitor = GetLlmMonitor::image(
            &base_url,
            "test-key",
            None,
            Duration::from_secs(60),
            Duration::from_secs(2),
            ImageProbeConfig {
                model: "test-image".to_owned(),
                prompt: "blue dot".to_owned(),
                size: "1024x1024".to_owned(),
                quality: Some("low".to_owned()),
                response_format: Some("url".to_owned()),
            },
        )?;

        let outcome = monitor.check().await;
        let requests = requests.await?;
        server.await??;
        let discovery_request = request_at(&requests, 0)?;
        let image_request = request_at(&requests, 1)?;

        assert_eq!(outcome.health, Health::Healthy);
        assert!(discovery_request.starts_with("GET /v1/models HTTP/1.1"));
        assert!(image_request.starts_with("POST /v1/images/generations HTTP/1.1"));
        assert!(image_request.contains("\"quality\":\"low\""));
        assert!(image_request.contains("\"response_format\":\"url\""));
        Ok(())
    }

    #[tokio::test]
    async fn image_probe_rejects_an_unadvertised_model() -> Result<(), Box<dyn Error>> {
        let (base_url, requests, server) =
            mock_server(vec![("200 OK", r#"{"object":"list","data":[]}"#)]).await?;
        let monitor = GetLlmMonitor::image(
            &base_url,
            "test-key",
            None,
            Duration::from_secs(600),
            Duration::from_secs(2),
            ImageProbeConfig {
                model: "gpt-image-2".to_owned(),
                prompt: "dot".to_owned(),
                size: "1024x1024".to_owned(),
                quality: Some("low".to_owned()),
                response_format: None,
            },
        )?;

        let outcome = monitor.check().await;
        let requests = requests.await?;
        server.await??;

        assert_eq!(outcome.health, Health::Unhealthy);
        assert!(outcome.summary.contains("gpt-image-2"));
        assert!(outcome.summary.contains("not advertised"));
        assert_eq!(requests.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn non_success_status_is_unhealthy() -> Result<(), Box<dyn Error>> {
        let (base_url, requests, server) = mock_server(vec![(
            "503 Service Unavailable",
            r#"{"error":{"message":"no healthy upstream"}}"#,
        )])
        .await?;
        let monitor = GetLlmMonitor::text(
            &base_url,
            "test-key",
            None,
            Duration::from_secs(60),
            Duration::from_secs(2),
            TextProbeConfig {
                model: "test-text".to_owned(),
                prompt: "ping".to_owned(),
                max_tokens: 1,
            },
        )?;

        let outcome = monitor.check().await;
        let _ = requests.await?;
        server.await??;

        assert_eq!(outcome.health, Health::Unhealthy);
        assert!(outcome.summary.contains("HTTP 503"));
        assert!(outcome.summary.contains("no healthy upstream"));
        Ok(())
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
        Ok((format!("http://{address}/v1"), request_rx, server))
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
