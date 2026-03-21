use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "acp-gateway", about = "ACP gateway with Cedar authorization and agent process pooling")]
pub struct Config {
    /// Command to spawn ACP agent processes (e.g. "kiro cli acp")
    #[arg(long, required = true)]
    pub agent_cmd: String,

    /// Load balancing strategy
    #[arg(long, value_enum, default_value = "least-connections")]
    pub strategy: Strategy,

    /// Pool size (for least-connections: fixed size; for dedicated: max processes)
    #[arg(long, default_value_t = 4)]
    pub pool_size: usize,

    /// Directory containing .cedar policy files (omit to disable authorization)
    #[arg(long)]
    pub policy_dir: Option<PathBuf>,

    /// Path to .cedarschema file for policy validation
    #[arg(long)]
    pub schema_file: Option<PathBuf>,

    /// Idle session timeout in seconds. Sessions with no activity for this long
    /// are evicted (process killed for dedicated, session unloaded for LC).
    /// The session can be transparently reloaded via loadSession on next prompt.
    /// 0 = no idle timeout.
    #[arg(long, default_value_t = 600)]
    pub idle_timeout_secs: u64,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

/// Load balancing strategy for routing sessions to agent processes.
///
/// ACP processes handle one prompt at a time with multiple sessions serialized.
///
/// - `LeastConnections`: fixed pool of `--pool-size` processes spawned at startup.
///   New sessions go to the process with fewest active sessions.
/// - `Dedicated`: one session per process, spawned on demand up to `--pool-size` max.
///   Process is reclaimed when its session ends.
#[derive(ValueEnum, Clone, Debug, PartialEq, Eq)]
pub enum Strategy {
    LeastConnections,
    Dedicated,
}
