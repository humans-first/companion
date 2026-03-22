use std::rc::Rc;
use agent_client_protocol::{self as acp, Agent as _};
use bytes::Bytes;
use futures::FutureExt as _;
use http_body_util::{BodyExt as _, Empty, Full};
use hyper::client::conn::http1;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::{connect, serve, ClientConfig, ConnectionFactory, HttpTransportConfig, TransportPeer};

#[derive(Clone, Default)]
struct MockAgent;

#[async_trait::async_trait(?Send)]
impl acp::Agent for MockAgent {
    async fn initialize(&self, args: acp::InitializeRequest) -> acp::Result<acp::InitializeResponse> {
        Ok(acp::InitializeResponse::new(args.protocol_version))
    }

    async fn authenticate(
        &self,
        _: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        _: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn load_session(
        &self,
        _: acp::LoadSessionRequest,
    ) -> acp::Result<acp::LoadSessionResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn set_session_mode(
        &self,
        _: acp::SetSessionModeRequest,
    ) -> acp::Result<acp::SetSessionModeResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn prompt(&self, _: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn cancel(&self, _: acp::CancelNotification) -> acp::Result<()> {
        Ok(())
    }

    async fn list_sessions(
        &self,
        _: acp::ListSessionsRequest,
    ) -> acp::Result<acp::ListSessionsResponse> {
        Ok(acp::ListSessionsResponse::new(vec![]))
    }

    async fn set_session_config_option(
        &self,
        _: acp::SetSessionConfigOptionRequest,
    ) -> acp::Result<acp::SetSessionConfigOptionResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_method(&self, _: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _: acp::ExtNotification) -> acp::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct MockClient;

#[async_trait::async_trait(?Send)]
impl acp::Client for MockClient {
    async fn request_permission(
        &self,
        _: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Cancelled,
        ))
    }

    async fn session_notification(&self, _: acp::SessionNotification) -> acp::Result<()> {
        Ok(())
    }

    async fn read_text_file(
        &self,
        _: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal(
        &self,
        _: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_method(&self, _: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _: acp::ExtNotification) -> acp::Result<()> {
        Ok(())
    }
}

struct ManualPeerHandles {
    outbound_writer: tokio::io::DuplexStream,
    inbound_reader: tokio::io::DuplexStream,
}

struct ManualServerHarness {
    addr: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
    handles_rx: mpsc::UnboundedReceiver<ManualPeerHandles>,
}

impl ManualServerHarness {
    async fn create_connection(&mut self) -> (String, ManualPeerHandles) {
        let create = http_request(
            self.addr,
            Request::builder()
                .method("POST")
                .uri("http://localhost/v1/acp/connections")
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await;
        assert_eq!(create.status(), StatusCode::CREATED);
        let create_body = create.into_body().collect().await.unwrap().to_bytes();
        let create_json: serde_json::Value = serde_json::from_slice(&create_body).unwrap();
        let connection_id = create_json["connection_id"].as_str().unwrap().to_string();
        let handles = self.handles_rx.recv().await.unwrap();
        (connection_id, handles)
    }
}

async fn start_manual_server(
    max_message_bytes: usize,
    max_buffered_messages: usize,
) -> ManualServerHarness {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (handles_tx, handles_rx) = mpsc::unbounded_channel();

    let factory: ConnectionFactory = Rc::new(move || {
        let handles_tx = handles_tx.clone();
        async move {
            let (transport_reader, outbound_writer) = tokio::io::duplex(64 * 1024);
            let (inbound_reader, transport_writer) = tokio::io::duplex(64 * 1024);

            handles_tx
                .send(ManualPeerHandles {
                    outbound_writer,
                    inbound_reader,
                })
                .map_err(|_| "failed to publish manual peer handles".to_string())?;

            Ok(TransportPeer::new(
                transport_reader.compat(),
                transport_writer.compat_write(),
                async {}.boxed_local(),
            ))
        }
        .boxed_local()
    });

    let server = tokio::task::spawn_local(async move {
        serve(
            listener,
            HttpTransportConfig {
                listen_addr: addr,
                max_message_bytes,
                max_buffered_messages,
            },
            factory,
        )
        .await
        .unwrap();
    });

    ManualServerHarness {
        addr,
        server,
        handles_rx,
    }
}

#[tokio::test]
async fn test_client_and_server_round_trip_initialize() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let factory: ConnectionFactory = Rc::new(move || {
                async move {
                    let (http_side, agent_side) = tokio::io::duplex(64 * 1024);
                    let (agent_read, agent_write) = tokio::io::split(agent_side);
                    let (http_read, http_write) = tokio::io::split(http_side);

                    let (_conn, io_task) = acp::AgentSideConnection::new(
                        MockAgent,
                        agent_write.compat_write(),
                        agent_read.compat(),
                        |fut| {
                            tokio::task::spawn_local(fut);
                        },
                    );

                    let io_handle = tokio::task::spawn_local(async move {
                        let _ = io_task.await;
                    });

                    Ok(TransportPeer::new(
                        http_read.compat(),
                        http_write.compat_write(),
                        async move {
                            io_handle.abort();
                        }
                        .boxed_local(),
                    ))
                }
                .boxed_local()
            });

            let server = tokio::task::spawn_local(async move {
                serve(
                    listener,
                    HttpTransportConfig {
                        listen_addr: addr,
                        max_message_bytes: 64 * 1024,
                        max_buffered_messages: 16,
                    },
                    factory,
                )
                .await
                .unwrap();
            });

            let transport = connect(ClientConfig::new(format!("http://{addr}")))
                .await
                .unwrap();
            let TransportPeer {
                reader,
                writer,
                shutdown,
            } = transport;

            let (conn, io_task) = acp::ClientSideConnection::new(
                MockClient,
                writer,
                reader,
                |fut| {
                    tokio::task::spawn_local(fut);
                },
            );
            let io_handle = tokio::task::spawn_local(async move {
                let _ = io_task.await;
            });

            let response = conn
                .initialize(
                    acp::InitializeRequest::new(acp::ProtocolVersion::LATEST)
                        .client_info(acp::Implementation::new("test-client", "0.0.0")),
                )
                .await
                .unwrap();

            assert_eq!(response.protocol_version, acp::ProtocolVersion::LATEST);

            shutdown.await;
            io_handle.abort();
            server.abort();
        })
        .await;
}

#[tokio::test]
async fn test_server_buffers_messages_before_stream_attach() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let factory: ConnectionFactory = Rc::new(move || {
                async move {
                    let (http_side, agent_side) = tokio::io::duplex(64 * 1024);
                    let (agent_read, agent_write) = tokio::io::split(agent_side);
                    let (http_read, http_write) = tokio::io::split(http_side);

                    let (_conn, io_task) = acp::AgentSideConnection::new(
                        MockAgent,
                        agent_write.compat_write(),
                        agent_read.compat(),
                        |fut| {
                            tokio::task::spawn_local(fut);
                        },
                    );

                    let io_handle = tokio::task::spawn_local(async move {
                        let _ = io_task.await;
                    });

                    Ok(TransportPeer::new(
                        http_read.compat(),
                        http_write.compat_write(),
                        async move {
                            io_handle.abort();
                        }
                        .boxed_local(),
                    ))
                }
                .boxed_local()
            });

            let server = tokio::task::spawn_local(async move {
                serve(
                    listener,
                    HttpTransportConfig {
                        listen_addr: addr,
                        max_message_bytes: 64 * 1024,
                        max_buffered_messages: 16,
                    },
                    factory,
                )
                .await
                .unwrap();
            });

            let create = http_request(
                addr,
                Request::builder()
                    .method("POST")
                    .uri("http://localhost/v1/acp/connections")
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await;
            let create_body = create.into_body().collect().await.unwrap().to_bytes();
            let create_json: serde_json::Value = serde_json::from_slice(&create_body).unwrap();
            let connection_id = create_json["connection_id"].as_str().unwrap().to_string();

            let initialize = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "initialize",
                "params": {
                    "protocolVersion": acp::ProtocolVersion::LATEST,
                    "clientInfo": {
                        "name": "test-client",
                        "version": "0.0.0"
                    }
                }
            })
            .to_string();

            let post = http_request(
                addr,
                Request::builder()
                    .method("POST")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(initialize)))
                    .unwrap(),
            )
            .await;
            assert_eq!(post.status(), StatusCode::ACCEPTED);

            let mut stream_response = http_request(
                addr,
                Request::builder()
                    .method("GET")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}/stream"))
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await;
            assert_eq!(stream_response.status(), StatusCode::OK);

            let frame = stream_response.body_mut().frame().await.unwrap().unwrap();
            let body = frame.into_data().unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            assert!(text.contains("\"id\":9"));

            server.abort();
        })
        .await;
}

#[tokio::test]
async fn test_connect_rejects_zero_max_message_bytes() {
    let err = match connect(ClientConfig {
        base_url: "http://127.0.0.1:1".to_string(),
        max_message_bytes: 0,
    })
    .await
    {
        Ok(_) => panic!("expected zero max_message_bytes to be rejected"),
        Err(err) => err,
    };

    assert!(err.contains("max_message_bytes"));
}

#[tokio::test]
async fn test_connect_rejects_non_http_base_url() {
    let err = match connect(ClientConfig::new("https://example.com"))
        .await
    {
        Ok(_) => panic!("expected non-http base_url to be rejected"),
        Err(err) => err,
    };

    assert!(err.contains("only http is supported"));
}

#[tokio::test]
async fn test_post_message_delivers_payload_to_transport_peer() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut harness = start_manual_server(64 * 1024, 16).await;
            let (connection_id, handles) = harness.create_connection().await;

            let payload = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "ping"
            })
            .to_string();

            let post = http_request(
                harness.addr,
                Request::builder()
                    .method("POST")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(payload.clone())))
                    .unwrap(),
            )
            .await;
            assert_eq!(post.status(), StatusCode::ACCEPTED);

            let mut reader = tokio::io::BufReader::new(handles.inbound_reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(line.trim_end(), payload);

            harness.server.abort();
        })
        .await;
}

#[tokio::test]
async fn test_server_rejects_invalid_json_message_body() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut harness = start_manual_server(64 * 1024, 16).await;
            let (connection_id, _handles) = harness.create_connection().await;

            let post = http_request(
                harness.addr,
                Request::builder()
                    .method("POST")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from_static(b"{not-json}")))
                    .unwrap(),
            )
            .await;
            assert_eq!(post.status(), StatusCode::BAD_REQUEST);

            harness.server.abort();
        })
        .await;
}

#[tokio::test]
async fn test_server_rejects_oversized_message_body() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut harness = start_manual_server(32, 16).await;
            let (connection_id, _handles) = harness.create_connection().await;

            let post = http_request(
                harness.addr,
                Request::builder()
                    .method("POST")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(
                        serde_json::json!({"payload": "x".repeat(128)}).to_string(),
                    )))
                    .unwrap(),
            )
            .await;
            assert_eq!(post.status(), StatusCode::PAYLOAD_TOO_LARGE);

            harness.server.abort();
        })
        .await;
}

#[tokio::test]
async fn test_server_rejects_second_stream_for_same_connection() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut harness = start_manual_server(64 * 1024, 16).await;
            let (connection_id, _handles) = harness.create_connection().await;

            let (mut first_stream, first_handle) = open_stream_request(
                harness.addr,
                format!("http://localhost/v1/acp/connections/{connection_id}/stream"),
            )
            .await;
            assert_eq!(first_stream.status(), StatusCode::OK);

            let second_stream = http_request(
                harness.addr,
                Request::builder()
                    .method("GET")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}/stream"))
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await;
            assert_eq!(second_stream.status(), StatusCode::CONFLICT);

            first_handle.abort();
            let _ = first_stream.body_mut().frame().await;
            harness.server.abort();
        })
        .await;
}

#[tokio::test]
async fn test_delete_connection_makes_subsequent_requests_unknown() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut harness = start_manual_server(64 * 1024, 16).await;
            let (connection_id, _handles) = harness.create_connection().await;

            let delete = http_request(
                harness.addr,
                Request::builder()
                    .method("DELETE")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}"))
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await;
            assert_eq!(delete.status(), StatusCode::NO_CONTENT);

            let post = http_request(
                harness.addr,
                Request::builder()
                    .method("POST")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}/messages"))
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from_static(br#"{"jsonrpc":"2.0"}"#)))
                    .unwrap(),
            )
            .await;
            assert_eq!(post.status(), StatusCode::NOT_FOUND);

            let stream = http_request(
                harness.addr,
                Request::builder()
                    .method("GET")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}/stream"))
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await;
            assert_eq!(stream.status(), StatusCode::NOT_FOUND);

            harness.server.abort();
        })
        .await;
}

#[tokio::test]
async fn test_stream_keeps_only_latest_buffered_messages() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut harness = start_manual_server(64 * 1024, 2).await;
            let (connection_id, mut handles) = harness.create_connection().await;

            handles.outbound_writer.write_all(b"one\n").await.unwrap();
            handles.outbound_writer.write_all(b"two\n").await.unwrap();
            handles.outbound_writer.write_all(b"three\n").await.unwrap();
            handles.outbound_writer.shutdown().await.unwrap();

            let (stream_response, stream_handle) = open_stream_request(
                harness.addr,
                format!("http://localhost/v1/acp/connections/{connection_id}/stream"),
            )
            .await;
            assert_eq!(stream_response.status(), StatusCode::OK);

            let body = stream_response.into_body().collect().await.unwrap().to_bytes();
            let text = std::str::from_utf8(&body).unwrap();
            assert!(!text.contains("one\n"));
            assert!(text.contains("two\n"));
            assert!(text.contains("three\n"));

            stream_handle.abort();
            harness.server.abort();
        })
        .await;
}

#[tokio::test]
async fn test_stream_returns_gone_after_peer_reader_closes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut harness = start_manual_server(64 * 1024, 16).await;
            let (connection_id, handles) = harness.create_connection().await;
            drop(handles.outbound_writer);
            tokio::task::yield_now().await;

            let stream = http_request(
                harness.addr,
                Request::builder()
                    .method("GET")
                    .uri(format!("http://localhost/v1/acp/connections/{connection_id}/stream"))
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await;
            assert_eq!(stream.status(), StatusCode::GONE);

            harness.server.abort();
        })
        .await;
}

async fn http_request<B>(addr: std::net::SocketAddr, request: Request<B>) -> hyper::Response<hyper::body::Incoming>
where
    B: hyper::body::Body<Data = Bytes> + Unpin + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let stream = TcpStream::connect(addr).await.unwrap();
    let io = TokioIo::new(stream);
    let (mut sender, connection) = http1::handshake(io).await.unwrap();
    let connection_handle = tokio::task::spawn_local(async move {
        let _ = connection.await;
    });
    let response = sender.send_request(request).await.unwrap();
    connection_handle.abort();
    response
}

async fn open_stream_request(
    addr: std::net::SocketAddr,
    uri: String,
) -> (
    hyper::Response<hyper::body::Incoming>,
    tokio::task::JoinHandle<()>,
) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let io = TokioIo::new(stream);
    let (mut sender, connection) = http1::handshake(io).await.unwrap();
    let connection_handle = tokio::task::spawn_local(async move {
        let _ = connection.await;
    });
    let response = sender
        .send_request(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap();
    (response, connection_handle)
}
