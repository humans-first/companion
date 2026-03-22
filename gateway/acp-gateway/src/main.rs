mod config;
mod error;
mod gateway;
mod policy;
mod pool;
mod service;
mod spawn;
#[cfg(test)]
mod tests;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use acp_http_transport::{ConnectionFactory, HttpTransportConfig, TransportPeer};
use clap::Parser;
use config::{Config, Strategy, Transport};
use futures::FutureExt as _;
use pool::{AgentPool, PoolConfig};
use service::GatewayService;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = Config::parse();

    if config.pool_size == 0 {
        return Err("--pool-size must be at least 1".into());
    }
    if config.http_max_message_bytes == 0 {
        return Err("--http-max-message-bytes must be at least 1".into());
    }
    if config.http_max_buffered_messages == 0 {
        return Err("--http-max-buffered-messages must be at least 1".into());
    }

    // Initialize tracing to stderr (stdout is the ACP transport).
    let filter = EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .json()
        .init();

    info!(
        agent_cmd = %config.agent_cmd,
        pool_size = config.pool_size,
        transport = ?config.transport,
        strategy = ?config.strategy,
        "starting acp-gateway"
    );

    // Load Cedar policies if configured.
    let policy_engine = if let Some(ref policy_dir) = config.policy_dir {
        let engine = policy::PolicyEngine::load(
            policy_dir,
            config.schema_file.as_deref(),
        )
        .map_err(|e| format!("failed to load policies: {e}"))?;
        Some(Arc::new(engine))
    } else {
        None
    };

    let idle_timeout = if config.idle_timeout_secs > 0 {
        Some(Duration::from_secs(config.idle_timeout_secs))
    } else {
        None
    };

    let config = Arc::new(config);
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            match config.transport {
                Transport::Stdio => run_stdio_gateway(config, policy_engine, idle_timeout).await,
                Transport::Http => run_http_gateway(config, policy_engine, idle_timeout).await,
            }
        })
        .await
}

async fn run_stdio_gateway(
    config: Arc<Config>,
    policy_engine: Option<Arc<policy::PolicyEngine>>,
    idle_timeout: Option<Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    let service = build_gateway_service(&config, policy_engine).await?;
    let pool = service.pool();
    let maintenance_task = spawn_pool_maintenance(pool.clone(), idle_timeout);
    let frontend_id = service.next_frontend_id();
    let gateway_agent = gateway::GatewayAgent::new(service.clone(), frontend_id.clone());
    let (upstream_conn, upstream_io) = agent_client_protocol::AgentSideConnection::new(
        gateway_agent,
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
        |fut| { tokio::task::spawn_local(fut); },
    );
    service.register_frontend(frontend_id.clone(), upstream_conn);

    let shutdown_pool = pool.clone();
    tokio::task::spawn_local(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("received shutdown signal");
                shutdown_pool.shutdown().await;
            }
            Err(e) => {
                warn!(error = %e, "failed to listen for shutdown signal");
            }
        }
    });

    if let Err(e) = upstream_io.await {
        error!(error = %e, "upstream I/O error");
    }

    service.unregister_frontend(&frontend_id);
    maintenance_task.abort();

    info!("gateway shutting down");
    pool.shutdown().await;
    Ok(())
}

async fn run_http_gateway(
    config: Arc<Config>,
    policy_engine: Option<Arc<policy::PolicyEngine>>,
    idle_timeout: Option<Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    let service = build_gateway_service(&config, policy_engine).await?;
    let pool = service.pool();
    let maintenance_task = spawn_pool_maintenance(pool.clone(), idle_timeout);

    let listener = tokio::net::TcpListener::bind(config.http_bind)
        .await
        .map_err(|e| format!("failed to bind {}: {e}", config.http_bind))?;

    let transport_config = HttpTransportConfig {
        listen_addr: config.http_bind,
        max_message_bytes: config.http_max_message_bytes,
        max_buffered_messages: config.http_max_buffered_messages,
    };
    let factory = build_gateway_connection_factory(config.clone(), service.clone());

    tokio::select! {
        result = acp_http_transport::serve(listener, transport_config, factory) => {
            maintenance_task.abort();
            result?;
        }
        signal = tokio::signal::ctrl_c() => {
            maintenance_task.abort();
            match signal {
                Ok(()) => {
                    info!("received shutdown signal");
                    pool.shutdown().await;
                }
                Err(e) => {
                    warn!(error = %e, "failed to listen for shutdown signal");
                }
            }
        }
    }
    Ok(())
}

fn build_gateway_connection_factory(
    config: Arc<Config>,
    service: GatewayService,
) -> ConnectionFactory {
    Rc::new(move || {
        let config = config.clone();
        let service = service.clone();

        async move {
            let (transport_side, gateway_side) = tokio::io::duplex(config.http_max_message_bytes * 2);
            let (gateway_read, gateway_write) = tokio::io::split(gateway_side);
            let (transport_read, transport_write) = tokio::io::split(transport_side);

            let frontend_id = service.next_frontend_id();
            let gateway_agent = gateway::GatewayAgent::new(service.clone(), frontend_id.clone());
            let (upstream_conn, upstream_io) = agent_client_protocol::AgentSideConnection::new(
                gateway_agent,
                gateway_write.compat_write(),
                gateway_read.compat(),
                |fut| { tokio::task::spawn_local(fut); },
            );
            service.register_frontend(frontend_id.clone(), upstream_conn);

            let upstream_task = tokio::task::spawn_local(async move {
                if let Err(err) = upstream_io.await {
                    error!(error = %err, "HTTP ACP upstream I/O error");
                }
            });

            Ok(TransportPeer::new(
                transport_read.compat(),
                transport_write.compat_write(),
                async move {
                    upstream_task.abort();
                    service.unregister_frontend(&frontend_id);
                }
                .boxed_local(),
            ))
        }
        .boxed_local()
    })
}

async fn build_gateway_service(
    config: &Config,
    policy_engine: Option<Arc<policy::PolicyEngine>>,
) -> Result<GatewayService, Box<dyn std::error::Error>> {
    let pool = AgentPool::new(PoolConfig {
        strategy: config.strategy.clone(),
        pool_size: config.pool_size,
        agent_cmd: config.agent_cmd.clone(),
    });
    let service = GatewayService::new(pool, policy_engine);
    spawn_initial_backends(&service, config).await?;
    Ok(service)
}

async fn spawn_initial_backends(
    service: &GatewayService,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
    match config.strategy {
        Strategy::LeastConnections => {
            for i in 0..config.pool_size {
                spawn::spawn_backend(service, &config.agent_cmd, i).await?;
            }
        }
        Strategy::Dedicated => {
            info!("dedicated mode: backends will spawn on demand");
        }
    }
    Ok(())
}

fn spawn_pool_maintenance(
    pool: AgentPool,
    idle_timeout: Option<Duration>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            tick += 1;

            if let Some(timeout) = idle_timeout {
                let evicted = pool.evict_idle_sessions(timeout);
                if !evicted.is_empty() {
                    info!(count = evicted.len(), "reaped idle sessions");
                }
            }
            pool.purge_stale_evicted(1000);

            if tick.is_multiple_of(10) {
                let status = pool.pool_status();
                info!(status = %status, "pool health");
            }
        }
    })
}
