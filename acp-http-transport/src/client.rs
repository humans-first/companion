use std::net::ToSocketAddrs;

use bytes::Bytes;
use futures::future::LocalBoxFuture;
use futures::FutureExt as _;
use http_body_util::{BodyExt as _, Empty, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper::header::CONTENT_TYPE;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{debug, warn};

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
    let connection = create_connection(&endpoint).await?;
    let (stream_body, stream_connection_task) =
        open_stream(&endpoint, &connection.stream_path).await?;

    let buffer_size = config.max_message_bytes.saturating_mul(2).max(1024);
    let (peer_reader, mut stream_writer) = tokio::io::duplex(buffer_size);
    let (mut message_reader, peer_writer) = tokio::io::duplex(buffer_size);

    let stream_task = tokio::task::spawn_local(async move {
        let mut body = stream_body;

        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        if let Err(err) = stream_writer.write_all(&data).await {
                            warn!(error = %err, "failed writing HTTP stream frame into transport");
                            break;
                        }
                    }
                }
                Some(Err(err)) => {
                    warn!(error = %err, "HTTP ACP stream failed");
                    break;
                }
                None => break,
            }
        }

        let _ = stream_writer.shutdown().await;
    });

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

                    if let Err(err) =
                        post_message(&endpoint_for_messages, &messages_path, Bytes::from(buffer.clone())).await
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

    let endpoint_for_shutdown = endpoint.clone();
    let connection_id = connection.connection_id;
    let shutdown: LocalBoxFuture<'static, ()> = async move {
        message_task.abort();
        stream_task.abort();
        stream_connection_task.abort();
        let _ = delete_connection(&endpoint_for_shutdown, &connection_id).await;
    }
    .boxed_local();

    Ok(TransportPeer::new(
        peer_reader.compat(),
        peer_writer.compat_write(),
        shutdown,
    ))
}

#[derive(Clone)]
struct Endpoint {
    base_url: String,
    host: String,
    port: u16,
}

impl Endpoint {
    fn parse(base_url: &str) -> Result<Self, String> {
        let normalized = base_url.trim_end_matches('/').to_string();
        let uri: Uri = normalized
            .parse()
            .map_err(|e| format!("invalid base_url '{base_url}': {e}"))?;
        let scheme = uri
            .scheme_str()
            .ok_or_else(|| "base_url must include a scheme".to_string())?;
        if scheme != "http" {
            return Err(format!("unsupported scheme '{scheme}', only http is supported"));
        }
        if uri.query().is_some() {
            return Err("base_url must not include a query".to_string());
        }
        let host = uri
            .host()
            .ok_or_else(|| "base_url must include a host".to_string())?
            .to_string();
        let port = uri.port_u16().unwrap_or(80);
        Ok(Self {
            base_url: normalized,
            host,
            port,
        })
    }

    fn uri(&self, path: &str) -> Result<Uri, String> {
        format!("{}{}", self.base_url, path)
            .parse()
            .map_err(|e| format!("failed to build URI for path '{path}': {e}"))
    }

    async fn connect_stream(&self) -> Result<TcpStream, String> {
        let mut addrs = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| format!("failed to resolve {}:{}: {e}", self.host, self.port))?;
        let addr = addrs
            .next()
            .ok_or_else(|| format!("no socket addresses found for {}:{}", self.host, self.port))?;
        TcpStream::connect(addr)
            .await
            .map_err(|e| format!("failed to connect to {}: {e}", addr))
    }
}

#[derive(Deserialize)]
struct CreateConnectionResponse {
    connection_id: String,
    messages_path: String,
    stream_path: String,
}

async fn create_connection(endpoint: &Endpoint) -> Result<CreateConnectionResponse, String> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(endpoint.uri("/v1/acp/connections")?)
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("failed to build create-connection request: {e}"))?;
    let response = send_request(endpoint, request).await?;
    let status = response.status();
    let body = collect_body(response).await?;
    if status != StatusCode::CREATED {
        return Err(format!(
            "create-connection failed with status {}: {}",
            status,
            String::from_utf8_lossy(&body)
        ));
    }
    serde_json::from_slice(&body).map_err(|e| format!("failed to parse create-connection response: {e}"))
}

async fn open_stream(
    endpoint: &Endpoint,
    stream_path: &str,
) -> Result<(Incoming, tokio::task::JoinHandle<()>), String> {
    let stream = endpoint.connect_stream().await?;
    let io = TokioIo::new(stream);
    let (mut sender, connection) = http1::handshake(io)
        .await
        .map_err(|e| format!("failed to open HTTP stream connection: {e}"))?;
    let connection_task = tokio::task::spawn_local(async move {
        if let Err(err) = connection.await {
            debug!(error = %err, "HTTP stream connection closed");
        }
    });
    let request = Request::builder()
        .method(Method::GET)
        .uri(endpoint.uri(stream_path)?)
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("failed to build stream request: {e}"))?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| format!("failed to open stream: {e}"))?;
    if response.status() != StatusCode::OK {
        let status = response.status();
        let body = collect_body(response).await?;
        connection_task.abort();
        return Err(format!(
            "stream connection failed with status {}: {}",
            status,
            String::from_utf8_lossy(&body)
        ));
    }
    Ok((response.into_body(), connection_task))
}

async fn post_message(endpoint: &Endpoint, messages_path: &str, body: Bytes) -> Result<(), String> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(endpoint.uri(messages_path)?)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(body))
        .map_err(|e| format!("failed to build message request: {e}"))?;
    let response = send_request(endpoint, request).await?;
    let status = response.status();
    let response_body = collect_body(response).await?;
    if status != StatusCode::ACCEPTED {
        return Err(format!(
            "message post failed with status {}: {}",
            status,
            String::from_utf8_lossy(&response_body)
        ));
    }
    Ok(())
}

async fn delete_connection(endpoint: &Endpoint, connection_id: &str) -> Result<(), String> {
    let request = Request::builder()
        .method(Method::DELETE)
        .uri(endpoint.uri(&format!("/v1/acp/connections/{connection_id}"))?)
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("failed to build delete request: {e}"))?;
    let response = send_request(endpoint, request).await?;
    let status = response.status();
    let body = collect_body(response).await?;
    match status {
        StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(()),
        _ => Err(format!(
            "delete connection failed with status {}: {}",
            status,
            String::from_utf8_lossy(&body)
        )),
    }
}

async fn send_request<B>(endpoint: &Endpoint, request: Request<B>) -> Result<Response<Incoming>, String>
where
    B: hyper::body::Body<Data = Bytes> + Unpin + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let stream = endpoint.connect_stream().await?;
    let io = TokioIo::new(stream);
    let (mut sender, connection) = http1::handshake(io)
        .await
        .map_err(|e| format!("failed to open HTTP connection: {e}"))?;
    let connection_task = tokio::task::spawn_local(async move {
        if let Err(err) = connection.await {
            debug!(error = %err, "HTTP request connection closed");
        }
    });
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;
    connection_task.abort();
    Ok(response)
}

async fn collect_body(response: Response<Incoming>) -> Result<Bytes, String> {
    response
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|e| format!("failed to read HTTP response body: {e}"))
}

fn trim_newline(buffer: &mut Vec<u8>) {
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
    }
    if buffer.last() == Some(&b'\r') {
        buffer.pop();
    }
}
