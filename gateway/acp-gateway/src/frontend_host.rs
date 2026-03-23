use std::collections::HashMap;
use std::sync::Arc;

use acp_http_transport::{ConnectionFactory, TransportPeer};
use futures::FutureExt as _;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::error;

use crate::gateway;
use crate::service::GatewayService;

enum FrontendRequest {
    Create {
        reply: oneshot::Sender<Result<TransportPeer, String>>,
    },
    Close {
        frontend_id: String,
    },
}

pub fn start(service: GatewayService, buffer_size: usize) -> ConnectionFactory {
    let (tx, mut rx) = mpsc::unbounded_channel::<FrontendRequest>();
    let control_tx = tx.clone();

    tokio::task::spawn_local(async move {
        let mut upstream_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

        while let Some(request) = rx.recv().await {
            match request {
                FrontendRequest::Create { reply } => {
                    let (transport_side, gateway_side) = tokio::io::duplex(buffer_size);
                    let (gateway_read, gateway_write) = tokio::io::split(gateway_side);
                    let (transport_read, transport_write) = tokio::io::split(transport_side);

                    let frontend_id = service.next_frontend_id();
                    let gateway_agent =
                        gateway::GatewayAgent::new(service.clone(), frontend_id.clone());
                    let (upstream_conn, upstream_io) = agent_client_protocol::AgentSideConnection::new(
                        gateway_agent,
                        gateway_write.compat_write(),
                        gateway_read.compat(),
                        |fut| {
                            tokio::task::spawn_local(fut);
                        },
                    );
                    service.register_frontend(frontend_id.clone(), upstream_conn);

                    let task_frontend_id = frontend_id.clone();
                    let upstream_task = tokio::task::spawn_local(async move {
                        if let Err(err) = upstream_io.await {
                            error!(frontend_id = %task_frontend_id, error = %err, "HTTP ACP upstream I/O error");
                        }
                    });
                    upstream_tasks.insert(frontend_id.clone(), upstream_task);

                    let shutdown_tx = control_tx.clone();
                    let shutdown_frontend_id = frontend_id.clone();
                    let peer = TransportPeer::new(
                        transport_read.compat(),
                        transport_write.compat_write(),
                        async move {
                            let _ = shutdown_tx.send(FrontendRequest::Close {
                                frontend_id: shutdown_frontend_id,
                            });
                        }
                        .boxed(),
                    );

                    if reply.send(Ok(peer)).is_err() {
                        if let Some(task) = upstream_tasks.remove(&frontend_id) {
                            task.abort();
                        }
                        service.unregister_frontend(&frontend_id);
                    }
                }
                FrontendRequest::Close { frontend_id } => {
                    if let Some(task) = upstream_tasks.remove(&frontend_id) {
                        task.abort();
                    }
                    service.unregister_frontend(&frontend_id);
                }
            }
        }

        for (frontend_id, task) in upstream_tasks.drain() {
            task.abort();
            service.unregister_frontend(&frontend_id);
        }
    });

    let request_tx = Arc::new(tx);
    Arc::new(move || {
        let request_tx = request_tx.clone();
        async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            request_tx
                .send(FrontendRequest::Create { reply: reply_tx })
                .map_err(|_| "gateway frontend host is closed".to_string())?;
            reply_rx
                .await
                .map_err(|_| "gateway frontend host dropped the create reply".to_string())?
        }
        .boxed()
    })
}
