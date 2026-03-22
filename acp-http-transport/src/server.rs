use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex as StdMutex};

use bytes::Bytes;
use futures::future::LocalBoxFuture;
use futures::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use futures::lock::Mutex;
use futures::stream;
use http_body_util::{BodyExt as _, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{BoxAsyncRead, BoxAsyncWrite, TransportPeer};

type HttpBody = http_body_util::combinators::UnsyncBoxBody<Bytes, Infallible>;

#[derive(Clone, Debug)]
pub struct HttpTransportConfig {
    pub listen_addr: SocketAddr,
    pub max_message_bytes: usize,
    pub max_buffered_messages: usize,
}

pub type ConnectionFactory =
    Rc<dyn Fn() -> LocalBoxFuture<'static, Result<TransportPeer, String>>>;

pub async fn serve(
    listener: TcpListener,
    config: HttpTransportConfig,
    factory: ConnectionFactory,
) -> std::io::Result<()> {
    let local_addr = listener.local_addr()?;
    info!(
        configured_addr = %config.listen_addr,
        listen_addr = %local_addr,
        "HTTP ACP transport listening"
    );

    let state = Rc::new(HttpTransportState::new(config, factory));

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();

        tokio::task::spawn_local(async move {
            let service = service_fn(move |request| handle_request(state.clone(), request));

            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                warn!(peer_addr = %peer_addr, error = %err, "HTTP ACP transport connection error");
            }
        });
    }
}

struct HttpTransportState {
    config: HttpTransportConfig,
    factory: ConnectionFactory,
    connections: RefCell<HashMap<String, Rc<HttpTransportConnection>>>,
}

impl HttpTransportState {
    fn new(config: HttpTransportConfig, factory: ConnectionFactory) -> Self {
        Self {
            config,
            factory,
            connections: RefCell::new(HashMap::new()),
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
        self.connections.borrow_mut().insert(id.clone(), connection);
        Ok(id)
    }

    fn get_connection(&self, id: &str) -> Option<Rc<HttpTransportConnection>> {
        self.connections.borrow().get(id).cloned()
    }

    async fn delete_connection(&self, id: &str) -> bool {
        let connection = self.connections.borrow_mut().remove(id);
        if let Some(connection) = connection {
            connection.close().await;
            true
        } else {
            false
        }
    }
}

struct HttpTransportConnection {
    writer: Mutex<Option<BoxAsyncWrite>>,
    outbound: Arc<OutboundBuffer>,
    shutdown: RefCell<Option<LocalBoxFuture<'static, ()>>>,
    io_tasks: RefCell<Vec<tokio::task::JoinHandle<()>>>,
    max_message_bytes: usize,
}

impl HttpTransportConnection {
    fn new(
        peer: TransportPeer,
        max_message_bytes: usize,
        max_buffered_messages: usize,
    ) -> Rc<Self> {
        let outbound = Arc::new(OutboundBuffer::new(max_buffered_messages));
        let this = Rc::new(Self {
            writer: Mutex::new(Some(peer.writer)),
            outbound: outbound.clone(),
            shutdown: RefCell::new(Some(peer.shutdown)),
            io_tasks: RefCell::new(Vec::new()),
            max_message_bytes,
        });

        let reader = peer.reader;
        let connection = this.clone();
        let pump_task = tokio::task::spawn_local(async move {
            connection.pump_outbound(reader).await;
        });
        this.io_tasks.borrow_mut().push(pump_task);

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

        for task in self.io_tasks.borrow_mut().drain(..) {
            task.abort();
        }

        self.outbound.close();

        let shutdown = self.shutdown.borrow_mut().take();
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

        let shutdown = self.shutdown.borrow_mut().take();
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

async fn handle_request(
    state: Rc<HttpTransportState>,
    request: Request<Incoming>,
) -> Result<Response<HttpBody>, Infallible> {
    let response = match (request.method(), request.uri().path()) {
        (&Method::POST, "/v1/acp/connections") => create_connection_response(state).await,
        _ => match parse_connection_route(request.uri().path()) {
            Some(ConnectionRoute::Stream(id)) if request.method() == Method::GET => {
                stream_response(state, id).await
            }
            Some(ConnectionRoute::Messages(id)) if request.method() == Method::POST => {
                post_message_response(state, id, request).await
            }
            Some(ConnectionRoute::Connection(id)) if request.method() == Method::DELETE => {
                delete_connection_response(state, id).await
            }
            _ => response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", "not found"),
        },
    };
    Ok(response)
}

async fn create_connection_response(state: Rc<HttpTransportState>) -> Response<HttpBody> {
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
                hyper::header::HeaderValue::from_str(&format!("/v1/acp/connections/{id}"))
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

async fn stream_response(state: Rc<HttpTransportState>, id: String) -> Response<HttpBody> {
    let Some(connection) = state.get_connection(&id) else {
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
            .map(|message| (Ok::<_, Infallible>(Frame::data(message)), (buffer, lease)))
    });

    let mut response = Response::new(http_body_util::combinators::UnsyncBoxBody::new(
        StreamBody::new(stream),
    ));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/x-ndjson"),
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store"),
    );
    response
}

async fn post_message_response(
    state: Rc<HttpTransportState>,
    id: String,
    request: Request<Incoming>,
) -> Response<HttpBody> {
    let Some(connection) = state.get_connection(&id) else {
        return response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", "unknown connection");
    };

    let body = match request.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(err) => {
            return response(
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8",
                format!("failed to read body: {err}"),
            )
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

async fn delete_connection_response(state: Rc<HttpTransportState>, id: String) -> Response<HttpBody> {
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
) -> Response<HttpBody> {
    let mut response = Response::new(http_body_util::combinators::UnsyncBoxBody::new(
        Full::new(body.into()),
    ));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static(content_type),
    );
    response
}

enum ConnectionRoute {
    Connection(String),
    Messages(String),
    Stream(String),
}

fn parse_connection_route(path: &str) -> Option<ConnectionRoute> {
    let prefix = "/v1/acp/connections/";
    let remainder = path.strip_prefix(prefix)?;
    let mut parts = remainder.split('/');
    let id = parts.next()?.to_string();
    match parts.next() {
        None => Some(ConnectionRoute::Connection(id)),
        Some("messages") if parts.next().is_none() => Some(ConnectionRoute::Messages(id)),
        Some("stream") if parts.next().is_none() => Some(ConnectionRoute::Stream(id)),
        _ => None,
    }
}
