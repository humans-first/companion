use std::path::PathBuf;

use agent_client_protocol::Client as _;
use clap::Parser;
use harness::{agent, config, llm, mcp, policy, sandbox, session_manager, session_store, tool_runtime};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "harness", about = "Minimalistic ACP agent harness")]
struct Cli {
    /// Path to the agent config file (JSON or YAML).
    #[arg(long)]
    config: PathBuf,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Initialize tracing to stderr (stdout is the ACP transport).
    let filter = EnvFilter::try_new(&cli.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .json()
        .init();

    let cfg = config::HarnessConfig::load(&cli.config)?;

    info!(
        name = %cfg.name,
        model = %cfg.model.model_id,
        mcp_servers = cfg.mcp_servers.len(),
        "starting harness"
    );

    let llm_client = llm::LlmClient::new(&cfg.model);
    let (notifier, mut notification_rx) = agent::session_notifier_channel();

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let mut runtime_manager = tool_runtime::ToolRuntimeManager::new();
            for (name, server_config) in &cfg.mcp_servers {
                info!(source = %name, command = %server_config.command, "registering MCP server");
                runtime_manager.register_factory(
                    name.clone(),
                    Box::new(mcp::McpToolSourceFactory::new(server_config.clone())),
                );
            }
            info!(
                configured_sources = runtime_manager.configured_sources().len(),
                "tool runtime manager initialized"
            );

            // Locate the sandbox runner script.
            let binary_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()));
            let runner_path = sandbox::find_runner(binary_dir.as_deref())
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            info!(runner = %runner_path.display(), "sandbox runner located");

            let sandbox_config = sandbox::SandboxConfig {
                timeout: std::time::Duration::from_secs(30),
                runner_path,
            };

            // Initialize Cedar policy engine from config.
            let policy_engine = policy::ToolPolicyEngine::from_policy_str(&cfg.tool_policy)
                .map_err(|e| -> Box<dyn std::error::Error> {
                    format!("failed to load tool policy: {e}").into()
                })?;
            info!("tool policy engine initialized");

            let session_manager = session_manager::SessionManager::new(
                cfg.clone(),
                Box::new(session_store::InMemorySessionStore::new()),
                runtime_manager,
            )
            .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;

            let harness_agent = agent::HarnessAgent::new(
                std::sync::Arc::new(llm_client),
                session_manager,
                sandbox_config,
                Some(policy_engine),
                notifier,
            );

            let (upstream_conn, upstream_io) = agent_client_protocol::AgentSideConnection::new(
                harness_agent,
                tokio::io::stdout().compat_write(),
                tokio::io::stdin().compat(),
                |fut| {
                    tokio::task::spawn_local(fut);
                },
            );

            tokio::task::spawn_local(async move {
                while let Some(envelope) = notification_rx.recv().await {
                    let result = upstream_conn
                        .session_notification(envelope.notification)
                        .await;
                    let _ = envelope.ack.send(result);
                }
            });

            tokio::pin!(upstream_io);

            tokio::select! {
                result = &mut upstream_io => {
                    if let Err(e) = result {
                        error!(error = %e, "ACP I/O error");
                    }
                }
                signal = tokio::signal::ctrl_c() => {
                    if let Err(e) = signal {
                        error!(error = %e, "failed to listen for shutdown signal");
                    } else {
                        info!("received shutdown signal");
                    }
                }
            }

            info!("harness shutting down");
            Ok(())
        })
        .await
}
