use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use axum::body::{to_bytes, Body};
use axum::extract::{Path, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use futures::stream;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{BoxAsyncRead, BoxAsyncWrite, TransportPeer};

#[derive(Clone, Debug)]
pub struct ServerTlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct HttpTransportConfig {
    pub listen_addr: SocketAddr,
    pub max_message_bytes: usize,
    pub max_buffered_messages: usize,
    pub tls: Option<ServerTlsConfig>,
}

pub type ConnectionFactory =
    Arc<dyn Fn() -> BoxFuture<'static, Result<TransportPeer, String>> + Send + Sync>;

pub async fn serve(
    listener: TcpListener,
    config: HttpTransportConfig,
    factory: ConnectionFactory,
) -> std::io::Result<()> {
    let local_addr = listener.local_addr()?;
    info!(
        configured_addr = %config.listen_addr,
        listen_addr = %local_addr,
        tls = config.tls.is_some(),
        "HTTP ACP transport listening"
    );

    let state = Arc::new(HttpTransportState::new(config.clone(), factory));
    let router = Router::new()
        .route("/v1/acp/connections", post(create_connection_response))
        .route(
            "/v1/acp/connections/{id}",
            delete(delete_connection_response),
        )
        .route(
            "/v1/acp/connections/{id}/messages",
            post(post_message_response),
        )
        .route("/v1/acp/connections/{id}/stream", get(stream_response))
        .with_state(state);

    if let Some(tls) = config.tls {
        let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            tls.cert_path,
            tls.key_path,
        )
        .await
        .map_err(std::io::Error::other)?;
        axum_server::from_tcp_rustls(listener.into_std()?, tls_config)
            .serve(router.into_make_service())
            .await
            .map_err(std::io::Error::other)
    } else {
        axum::serve(listener, router)
            .await
            .map_err(std::io::Error::other)
    }
}

struct HttpTransportState {
    config: HttpTransportConfig,
    factory: ConnectionFactory,
    connections: tokio::sync::Mutex<HashMap<String, Arc<HttpTransportConnection>>>,
}

impl HttpTransportState {
    fn new(config: HttpTransportConfig, factory: ConnectionFactory) -> Self {
        Self {
            config,
            factory,
            connections: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn create_connection(&self) -> Result<String, String> {
        let peer = (self.factory)().await?;
        let connection = HttpTransportConnection::new(
            peer,
            self.config.max_message_bytes,
            self.config.max_buffered_messages,
        );
        let id = Uuid::now_v7().to_string();
        self.connections.lock().await.insert(id.clone(), connection);
        Ok(id)
    }

    async fn get_connection(&self, id: &str) -> Option<Arc<HttpTransportConnection>> {
        self.connections.lock().await.get(id).cloned()
    }

    async fn delete_connection(&self, id: &str) -> bool {
        let connection = self.connections.lock().await.remove(id);
        if let Some(connection) = connection {
            connection.close().await;
            true
        } else {
            false
        }
    }
}

struct HttpTransportConnection {
    writer: tokio::sync::Mutex<Option<BoxAsyncWrite>>,
    outbound: Arc<OutboundBuffer>,
    shutdown: tokio::sync::Mutex<Option<BoxFuture<'static, ()>>>,
    io_tasks: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
    max_message_bytes: usize,
}

impl HttpTransportConnection {
    fn new(
        peer: TransportPeer,
        max_message_bytes: usize,
        max_buffered_messages: usize,
    ) -> Arc<Self> {
        let outbound = Arc::new(OutboundBuffer::new(max_buffered_messages));
        let this = Arc::new(Self {
            writer: tokio::sync::Mutex::new(Some(peer.writer)),
            outbound: outbound.clone(),
            shutdown: tokio::sync::Mutex::new(Some(peer.shutdown)),
            io_tasks: StdMutex::new(Vec::new()),
            max_message_bytes,
        });

        let reader = peer.reader;
        let connection = this.clone();
        let pump_task = tokio::spawn(async move {
            connection.pump_outbound(reader).await;
        });
        this.io_tasks.lock().unwrap().push(pump_task);

        this
    }

    async fn send_message(&self, message: Bytes) -> Result<(), &'static str> {
        if message.len() > self.max_message_bytes {
            return Err("message exceeds max size");
        }

        let mut writer = self.writer.lock().await;
        let writer = writer.as_mut().ok_or("connection is closed")?;

        writer
            .write_all(&message)
            .await
            .map_err(|_| "failed to write message")?;
        writer
            .write_all(b"\n")
            .await
            .map_err(|_| "failed to write message terminator")?;
        writer.flush().await.map_err(|_| "failed to flush message")?;
        Ok(())
    }

    async fn close(&self) {
        {
            let mut writer = self.writer.lock().await;
            if let Some(mut writer) = writer.take() {
                let _ = writer.close().await;
            }
        }

        {
            let mut io_tasks = self.io_tasks.lock().unwrap();
            for task in io_tasks.drain(..) {
                task.abort();
            }
        }

        self.outbound.close();

        let shutdown = self.shutdown.lock().await.take();
        if let Some(shutdown) = shutdown {
            shutdown.await;
        }
    }

    async fn pump_outbound(&self, reader: BoxAsyncRead) {
        let mut reader = BufReader::new(reader);
        let mut buffer = Vec::new();

        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer).await {
                Ok(0) => break,
                Ok(_) => {
                    if buffer.len() > self.max_message_bytes + 1 {
                        warn!(size = buffer.len(), "dropping oversized outbound ACP message");
                        break;
                    }
                    self.outbound.push(Bytes::from(buffer.clone()));
                }
                Err(err) => {
                    error!(error = %err, "failed reading outbound ACP stream");
                    break;
                }
            }
        }

        self.outbound.close();

        let shutdown = self.shutdown.lock().await.take();
        if let Some(shutdown) = shutdown {
            shutdown.await;
        }
    }
}

struct OutboundBuffer {
    state: StdMutex<OutboundState>,
    notify: Notify,
    max_buffered_messages: usize,
}

struct OutboundState {
    queue: VecDeque<Bytes>,
    stream_open: bool,
    closed: bool,
}

impl OutboundBuffer {
    fn new(max_buffered_messages: usize) -> Self {
        Self {
            state: StdMutex::new(OutboundState {
                queue: VecDeque::new(),
                stream_open: false,
                closed: false,
            }),
            notify: Notify::new(),
            max_buffered_messages,
        }
    }

    fn push(&self, message: Bytes) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        if state.queue.len() >= self.max_buffered_messages {
            state.queue.pop_front();
        }
        state.queue.push_back(message);
        drop(state);
        self.notify.notify_waiters();
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        drop(state);
        self.notify.notify_waiters();
    }

    fn claim_stream(self: &Arc<Self>) -> Result<StreamLease, &'static str> {
        let mut state = self.state.lock().unwrap();
        if state.stream_open {
            return Err("stream already open");
        }
        if state.closed && state.queue.is_empty() {
            return Err("connection is closed");
        }
        state.stream_open = true;
        Ok(StreamLease {
            buffer: self.clone(),
        })
    }

    fn release_stream(&self) {
        let mut state = self.state.lock().unwrap();
        state.stream_open = false;
        drop(state);
        self.notify.notify_waiters();
    }

    async fn next_message(&self) -> Option<Bytes> {
        loop {
            if let Some(message) = {
                let mut state = self.state.lock().unwrap();
                state.queue.pop_front()
            } {
                return Some(message);
            }

            if self.state.lock().unwrap().closed {
                return None;
            }

            self.notify.notified().await;
        }
    }
}

struct StreamLease {
    buffer: Arc<OutboundBuffer>,
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.buffer.release_stream();
    }
}

async fn create_connection_response(State(state): State<Arc<HttpTransportState>>) -> Response {
    match state.create_connection().await {
        Ok(id) => {
            let body = serde_json::json!({
                "connection_id": id,
                "messages_path": format!("/v1/acp/connections/{id}/messages"),
                "stream_path": format!("/v1/acp/connections/{id}/stream"),
            })
            .to_string();

            let mut response = response(StatusCode::CREATED, "application/json", body);
            if let Ok(value) =
                HeaderValue::from_str(&format!("/v1/acp/connections/{id}"))
            {
                response.headers_mut().insert(LOCATION, value);
            }
            response
        }
        Err(err) => response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain; charset=utf-8",
            err,
        ),
    }
}

async fn stream_response(
    State(state): State<Arc<HttpTransportState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(connection) = state.get_connection(&id).await else {
        return response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", "unknown connection");
    };

    let lease = match connection.outbound.claim_stream() {
        Ok(lease) => lease,
        Err("connection is closed") => {
            return response(
                StatusCode::GONE,
                "text/plain; charset=utf-8",
                "connection is closed",
            );
        }
        Err(err) => return response(StatusCode::CONFLICT, "text/plain; charset=utf-8", err),
    };

    let buffer = connection.outbound.clone();
    let stream = stream::unfold((buffer, lease), |(buffer, lease)| async move {
        buffer
            .next_message()
            .await
            .map(|message| (Ok::<_, Infallible>(message), (buffer, lease)))
    });

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response
}

async fn post_message_response(
    State(state): State<Arc<HttpTransportState>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let Some(connection) = state.get_connection(&id).await else {
        return response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", "unknown connection");
    };

    let body = match to_bytes(request.into_body(), state.config.max_message_bytes + 1).await {
        Ok(body) => body,
        Err(err) => {
            let text = err.to_string();
            if text.contains("length limit exceeded") {
                return response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "text/plain; charset=utf-8",
                    "message exceeds configured limit",
                );
            }
            return response(
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8",
                format!("failed to read body: {err}"),
            );
        }
    };

    if body.is_empty() {
        return response(StatusCode::BAD_REQUEST, "text/plain; charset=utf-8", "empty body");
    }

    if body.len() > state.config.max_message_bytes {
        return response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "text/plain; charset=utf-8",
            "message exceeds configured limit",
        );
    }

    if serde_json::from_slice::<serde_json::Value>(&body).is_err() {
        return response(
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            "body must be valid JSON",
        );
    }

    match connection.send_message(body).await {
        Ok(()) => response(StatusCode::ACCEPTED, "text/plain; charset=utf-8", ""),
        Err("message exceeds max size") => response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "text/plain; charset=utf-8",
            "message exceeds configured limit",
        ),
        Err(err) => response(StatusCode::GONE, "text/plain; charset=utf-8", err),
    }
}

async fn delete_connection_response(
    State(state): State<Arc<HttpTransportState>>,
    Path(id): Path<String>,
) -> Response {
    if state.delete_connection(&id).await {
        response(StatusCode::NO_CONTENT, "text/plain; charset=utf-8", "")
    } else {
        response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", "unknown connection")
    }
}

fn response(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Bytes>,
) -> Response {
    let mut response = Response::new(Body::from(body.into()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}
