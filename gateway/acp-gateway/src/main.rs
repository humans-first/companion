mod config;
mod error;
mod gateway;
mod policy;
mod pool;
mod spawn;
#[cfg(test)]
mod tests;

use clap::Parser;
use config::{Config, Strategy};
use pool::{AgentPool, PoolConfig};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = Config::parse();

    if config.pool_size == 0 {
        return Err("--pool-size must be at least 1".into());
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
        Some(engine)
    } else {
        None
    };

    let idle_timeout = if config.idle_timeout_secs > 0 {
        Some(std::time::Duration::from_secs(config.idle_timeout_secs))
    } else {
        None
    };

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let pool = AgentPool::new(PoolConfig {
                strategy: config.strategy.clone(),
                pool_size: config.pool_size,
                agent_cmd: config.agent_cmd.clone(),
            });

            // Spawn initial backends based on strategy.
            match config.strategy {
                Strategy::LeastConnections => {
                    for i in 0..config.pool_size {
                        spawn::spawn_backend(&pool, &config.agent_cmd, i).await?;
                    }
                }
                Strategy::Dedicated => {
                    // Dedicated mode: backends are spawned on demand.
                    info!("dedicated mode: backends will spawn on demand");
                }
            }

            // Create the agent-side connection to the upstream client.
            let gateway_agent = gateway::GatewayAgent::new(pool.clone(), policy_engine);
            let (upstream_conn, upstream_io) =
                agent_client_protocol::AgentSideConnection::new(
                    gateway_agent,
                    tokio::io::stdout().compat_write(),
                    tokio::io::stdin().compat(),
                    |fut| { tokio::task::spawn_local(fut); },
                );

            pool.set_upstream(upstream_conn);

            // Spawn periodic maintenance task: idle eviction, evicted map cleanup, health logging.
            {
                let reaper_pool = pool.clone();
                tokio::task::spawn_local(async move {
                    let mut tick = 0u64;
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        tick += 1;

                        if let Some(timeout) = idle_timeout {
                            let evicted = reaper_pool.evict_idle_sessions(timeout);
                            if !evicted.is_empty() {
                                info!(count = evicted.len(), "reaped idle sessions");
                            }
                        }
                        reaper_pool.purge_stale_evicted(1000);

                        // Log pool health every ~5 minutes (every 10 ticks of 30s).
                        if tick % 10 == 0 {
                            let status = reaper_pool.pool_status();
                            info!(status = %status, "pool health");
                        }
                    }
                });
            }

            // Spawn signal handler for graceful shutdown.
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

            // Run upstream I/O (blocks until stdin/stdout close).
            if let Err(e) = upstream_io.await {
                error!(error = %e, "upstream I/O error");
            }

            info!("gateway shutting down");
            pool.shutdown().await;
            Ok(())
        })
        .await
}
