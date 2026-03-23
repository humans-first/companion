use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use reqwest::header::CONTENT_TYPE;
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::warn;

use crate::TransportPeer;

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub base_url: String,
    pub max_message_bytes: usize,
}

impl ClientConfig {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            max_message_bytes: 1024 * 1024,
        }
    }
}

pub async fn connect(config: ClientConfig) -> Result<TransportPeer, String> {
    if config.max_message_bytes == 0 {
        return Err("max_message_bytes must be at least 1".to_string());
    }

    let endpoint = Endpoint::parse(&config.base_url)?;
    let client = build_http_client()?;
    let connection = create_connection(&client, &endpoint).await?;
    let stream_response = open_stream(&client, &endpoint, &connection.stream_path).await?;

    let buffer_size = config.max_message_bytes.saturating_mul(2).max(1024);
    let (peer_reader, mut stream_writer) = tokio::io::duplex(buffer_size);
    let (mut message_reader, peer_writer) = tokio::io::duplex(buffer_size);

    let stream_task = tokio::task::spawn_local(async move {
        let mut body = stream_response.bytes_stream();

        while let Some(frame) = body.next().await {
            match frame {
                Ok(data) => {
                    if let Err(err) = stream_writer.write_all(&data).await {
                        warn!(error = %err, "failed writing HTTP stream frame into transport");
                        break;
                    }
                }
                Err(err) => {
                    warn!(error = %err, "HTTP ACP stream failed");
                    break;
                }
            }
        }

        let _ = stream_writer.shutdown().await;
    });

    let client_for_messages = client.clone();
    let endpoint_for_messages = endpoint.clone();
    let messages_path = connection.messages_path.clone();
    let max_message_bytes = config.max_message_bytes;
    let message_task = tokio::task::spawn_local(async move {
        let mut reader = tokio::io::BufReader::new(&mut message_reader);
        let mut buffer = Vec::new();

        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer).await {
                Ok(0) => break,
                Ok(_) => {
                    trim_newline(&mut buffer);
                    if buffer.is_empty() {
                        continue;
                    }

                    if buffer.len() > max_message_bytes {
                        warn!(size = buffer.len(), "dropping oversized outbound ACP request");
                        break;
                    }

                    if let Err(err) = post_message(
                        &client_for_messages,
                        &endpoint_for_messages,
                        &messages_path,
                        Bytes::from(buffer.clone()),
                    )
                    .await
                    {
                        warn!(error = %err, "failed forwarding outbound ACP request over HTTP");
                        break;
                    }
                }
                Err(err) => {
                    warn!(error = %err, "failed reading outbound ACP request from transport");
                    break;
                }
            }
        }
    });

    let client_for_shutdown = client.clone();
    let endpoint_for_shutdown = endpoint.clone();
    let connection_id = connection.connection_id;
    let shutdown: BoxFuture<'static, ()> = async move {
        message_task.abort();
        stream_task.abort();
        let _ = delete_connection(&client_for_shutdown, &endpoint_for_shutdown, &connection_id).await;
    }
    .boxed();

    Ok(TransportPeer::new(
        peer_reader.compat(),
        peer_writer.compat_write(),
        shutdown,
    ))
}

#[derive(Clone)]
struct Endpoint {
    base_url: Url,
}

impl Endpoint {
    fn parse(base_url: &str) -> Result<Self, String> {
        let normalized = base_url.trim_end_matches('/');
        let url =
            Url::parse(normalized).map_err(|e| format!("invalid base_url '{base_url}': {e}"))?;
        match url.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(format!(
                    "unsupported scheme '{scheme}', only http and https are supported"
                ))
            }
        }
        if url.query().is_some() {
            return Err("base_url must not include a query".to_string());
        }
        if url.host_str().is_none() {
            return Err("base_url must include a host".to_string());
        }
        Ok(Self { base_url: url })
    }

    fn url(&self, path: &str) -> Result<Url, String> {
        self.base_url
            .join(path)
            .map_err(|e| format!("failed to build URL for path '{path}': {e}"))
    }
}

fn build_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

#[derive(Deserialize)]
struct CreateConnectionResponse {
    connection_id: String,
    messages_path: String,
    stream_path: String,
}

async fn create_connection(
    client: &reqwest::Client,
    endpoint: &Endpoint,
) -> Result<CreateConnectionResponse, String> {
    let response = client
        .post(endpoint.url("/v1/acp/connections")?)
        .send()
        .await
        .map_err(|e| format!("failed to create connection: {e}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read create-connection response body: {e}"))?;
    if status != StatusCode::CREATED {
        return Err(format!(
            "create-connection failed with status {}: {}",
            status,
            String::from_utf8_lossy(&body)
        ));
    }
    serde_json::from_slice(&body)
        .map_err(|e| format!("failed to parse create-connection response: {e}"))
}

async fn open_stream(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    stream_path: &str,
) -> Result<reqwest::Response, String> {
    let response = client
        .get(endpoint.url(stream_path)?)
        .send()
        .await
        .map_err(|e| format!("failed to open stream: {e}"))?;
    if response.status() != StatusCode::OK {
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("failed to read stream error body: {e}"))?;
        return Err(format!(
            "stream connection failed with status {}: {}",
            status,
            String::from_utf8_lossy(&body)
        ));
    }
    Ok(response)
}

async fn post_message(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    messages_path: &str,
    body: Bytes,
) -> Result<(), String> {
    let response = client
        .post(endpoint.url(messages_path)?)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("failed to post message: {e}"))?;
    let status = response.status();
    let response_body = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read message response body: {e}"))?;
    if status != StatusCode::ACCEPTED {
        return Err(format!(
            "message post failed with status {}: {}",
            status,
            String::from_utf8_lossy(&response_body)
        ));
    }
    Ok(())
}

async fn delete_connection(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    connection_id: &str,
) -> Result<(), String> {
    let response = client
        .delete(endpoint.url(&format!("/v1/acp/connections/{connection_id}"))?)
        .send()
        .await
        .map_err(|e| format!("failed to delete connection: {e}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read delete response body: {e}"))?;
    match status {
        StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(()),
        _ => Err(format!(
            "delete connection failed with status {}: {}",
            status,
            String::from_utf8_lossy(&body)
        )),
    }
}

fn trim_newline(buffer: &mut Vec<u8>) {
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
    }
    if buffer.last() == Some(&b'\r') {
        buffer.pop();
    }
}
