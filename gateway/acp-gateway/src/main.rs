mod config;
mod error;
mod gateway;
mod policy;
mod pool;
#[cfg(test)]
mod tests;

use std::os::unix::process::CommandExt as _;

use clap::Parser;
use config::{Config, Strategy};
use pool::{AgentPool, BackendSlot, PoolConfig};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = Config::parse();

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
                        spawn_backend(&pool, &config.agent_cmd, i).await?;
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

            // Spawn idle session reaper if configured.
            if let Some(timeout) = idle_timeout {
                let reaper_pool = pool.clone();
                tokio::task::spawn_local(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        let evicted = reaper_pool.evict_idle_sessions(timeout);
                        if !evicted.is_empty() {
                            info!(count = evicted.len(), "reaped idle sessions");
                        }
                        reaper_pool.purge_stale_evicted(1000);
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

/// Spawn a backend agent process and wire it into the pool.
pub(crate) async fn spawn_backend(
    pool: &AgentPool,
    agent_cmd: &str,
    slot_index: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let (program, args) = parse_cmd(agent_cmd)?;
    let mut child = std::process::Command::new(&program);
    child.args(&args);
    // Put child in its own process group so terminal signals don't kill it directly.
    child.process_group(0);

    let mut child = tokio::process::Command::from(child)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn '{}': {e}", agent_cmd))?;

    let child_stdin = child.stdin.take().expect("child stdin");
    let child_stdout = child.stdout.take().expect("child stdout");
    info!(slot_index, pid = ?child.id(), "spawned backend agent process");

    let gateway_client = gateway::GatewayClient::new(pool.clone());
    let (conn, io_task) = agent_client_protocol::ClientSideConnection::new(
        gateway_client,
        child_stdin.compat_write(),
        child_stdout.compat(),
        |fut| { tokio::task::spawn_local(fut); },
    );

    let slot = BackendSlot {
        connection: conn,
        child: Some(child),
        session_count: 0,
        alive: true,
    };
    pool.insert_slot(slot_index, slot);

    // Run I/O task; on completion, handle crash recovery.
    let crash_pool = pool.clone();
    let agent_cmd_owned = agent_cmd.to_string();
    tokio::task::spawn_local(async move {
        if let Err(e) = io_task.await {
            error!(slot_index, error = %e, "backend I/O error");
        }
        info!(slot_index, "backend connection closed");
        crash_pool.handle_backend_death(slot_index);

        // For LC, attempt respawn with exponential backoff.
        if crash_pool.strategy() == Strategy::LeastConnections {
            const MAX_RETRIES: u32 = 5;
            for attempt in 0..MAX_RETRIES {
                let delay = std::time::Duration::from_secs(1 << attempt.min(4)); // 1s, 2s, 4s, 8s, 16s
                info!(slot_index, attempt, delay_secs = delay.as_secs(), "respawning backend after delay");
                tokio::time::sleep(delay).await;

                match spawn_backend(&crash_pool, &agent_cmd_owned, slot_index).await {
                    Ok(()) => {
                        info!(slot_index, "backend respawned successfully");
                        return;
                    }
                    Err(e) => {
                        error!(slot_index, attempt, error = %e, "failed to respawn backend");
                    }
                }
            }
            error!(slot_index, "giving up on respawning backend after {MAX_RETRIES} attempts");
        }
    });

    Ok(())
}

fn parse_cmd(cmd: &str) -> std::result::Result<(String, Vec<String>), String> {
    let parts = shell_words::split(cmd)
        .map_err(|e| format!("cannot parse agent command: {e}"))?;
    if parts.is_empty() {
        return Err("empty agent command".to_string());
    }
    Ok((parts[0].clone(), parts[1..].to_vec()))
}
