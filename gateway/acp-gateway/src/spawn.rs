use std::os::unix::process::CommandExt as _;

use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{error, info};

use crate::config::Strategy;
use crate::gateway;
use crate::pool::BackendSlot;
use crate::service::GatewayService;

/// Spawn a backend agent process and wire it into the pool.
pub async fn spawn_backend(
    service: &GatewayService,
    agent_cmd: &str,
    slot_index: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let pool = service.pool();
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

    let gateway_client = gateway::GatewayClient::new(service.clone());
    let (conn, io_task) = agent_client_protocol::ClientSideConnection::new(
        gateway_client,
        child_stdin.compat_write(),
        child_stdout.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
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
    let crash_service = service.clone();
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

                match spawn_backend(&crash_service, &agent_cmd_owned, slot_index).await {
                    Ok(()) => {
                        // Replay stored init request to the new backend.
                        if let Err(e) = crash_pool.replay_init(slot_index).await {
                            error!(slot_index, error = %e, "failed to initialize respawned backend");
                            continue;
                        }
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

fn parse_cmd(cmd: &str) -> Result<(String, Vec<String>), String> {
    let parts =
        shell_words::split(cmd).map_err(|e| format!("cannot parse agent command: {e}"))?;
    if parts.is_empty() {
        return Err("empty agent command".to_string());
    }
    Ok((parts[0].clone(), parts[1..].to_vec()))
}
