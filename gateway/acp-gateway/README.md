# acp-gateway

An [ACP](https://github.com/anthropics/agent-client-protocol) gateway proxy that sits between an editor (Zed, VS Code, etc.) and one or more ACP agent processes. It provides session multiplexing, process pooling, idle eviction with transparent reload, crash recovery, Cedar-based authorization, and pool health monitoring.

```
┌────────┐         ┌─────────────┐        ┌─────────┐
│ Editor │◄──ACP──►│ acp-gateway │◄──ACP──►│ Agent 0 │
└────────┘  stdin/  │             │         └─────────┘
            stdout  │  session    │         ┌─────────┐
                    │  routing    │◄──ACP──►│ Agent 1 │
                    │  + Cedar    │         └─────────┘
                    │  authz      │         ┌─────────┐
                    │             │◄──ACP──►│ Agent N │
                    └─────────────┘         └─────────┘
```

The gateway exposes a single ACP connection on stdin/stdout. Internally it manages a pool of backend agent processes, each also connected via ACP over stdin/stdout. Session IDs are rewritten so the upstream client never sees backend-internal identifiers.

## Quick start

```sh
cargo build --release

# Dedicated mode (default for single-user): one process per session
acp-gateway --agent-cmd "kiro-cli acp -a" --strategy dedicated --pool-size 8

# Least-connections mode: fixed pool, sessions distributed by load
acp-gateway --agent-cmd "kiro-cli acp -a" --strategy least-connections --pool-size 4
```

In an editor like Zed, configure it as a custom agent:

```json
{
  "agent": {
    "type": "custom",
    "command": "acp-gateway",
    "args": ["--agent-cmd", "kiro-cli acp -a", "--strategy", "dedicated"]
  }
}
```

## Strategies

### Dedicated (recommended for most use cases)

One backend process per session, spawned on demand, killed when the session is evicted or closed. Best for isolation: each session gets its own process with independent state.

- `--pool-size` sets the maximum number of concurrent sessions/processes.
- Idle sessions are evicted after `--idle-timeout-secs` (default 600s). The process is gracefully terminated (SIGTERM, then SIGKILL after 500ms).
- Evicted sessions are transparently reloaded on the next prompt via `load_session` on a fresh backend.

### Least-connections

A fixed pool of `--pool-size` backend processes is spawned at startup. New sessions are routed to the process with the fewest active sessions. Best for high session throughput with shared backends.

- Backend processes are automatically respawned on crash (exponential backoff, up to 5 retries).
- Idle sessions are evicted but the backend process stays alive.

## Cedar authorization

Optionally gate `prompt` requests with [Cedar](https://www.cedarpolicy.com/) policies:

```sh
acp-gateway --agent-cmd "..." --policy-dir ./policies/ --schema-file ./policies/schema.cedarschema
```

The gateway extracts `principal`, `action`, and `resource` from the ACP request's `_meta` field and evaluates them against the loaded policies. If no policy permits the request, it is denied with error code `-32004`.

Example `_meta`:
```json
{
  "_meta": {
    "principal": "User::\"alice\"",
    "action": "Action::\"prompt\"",
    "resource": "Agent::\"kiro\""
  }
}
```

Example Cedar policy (`policies/allow-all.cedar`):
```
permit(principal, action, resource);
```

## CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--agent-cmd` | (required) | Shell command to spawn backend agents |
| `--strategy` | `least-connections` | `dedicated` or `least-connections` |
| `--pool-size` | `4` | Max processes (dedicated) or fixed pool size (LC) |
| `--policy-dir` | (none) | Directory of `.cedar` policy files |
| `--schema-file` | (none) | `.cedarschema` file for policy validation |
| `--idle-timeout-secs` | `600` | Idle session eviction timeout (0 = disabled) |
| `--log-level` | `info` | `trace`, `debug`, `info`, `warn`, `error` |

## Pool health

Query pool status at runtime via the `gateway/status` ACP extension method. The response includes slot states, session counts, and eviction stats:

```json
{
  "strategy": "Dedicated",
  "pool_size": 4,
  "alive_slots": 2,
  "dead_slots": 0,
  "empty_slots": 2,
  "active_sessions": 2,
  "evicted_sessions": 1,
  "in_flight_prompts": 0,
  "slots": [...]
}
```

The gateway also logs pool health every 5 minutes to stderr (JSON structured logging).

## Session lifecycle

```
new_session ──► register mapping (external UUID ↔ backend ID)
                assign to slot (LC: least loaded, Dedicated: spawn new)

prompt ────────► check Cedar policy
                 check prompt guard (one prompt per session at a time)
                 if session evicted: transparent reload via load_session
                 forward to backend with rewritten session ID

idle timeout ──► evict session to evicted map
                 Dedicated: graceful kill (SIGTERM → SIGKILL)
                 LC: decrement session count, keep process alive

next prompt ───► detect eviction, select new slot
                 load_session on new backend
                 re-register mapping, forward prompt

backend crash ─► move all sessions on that slot to evicted
                 clear in-flight prompts
                 LC: respawn with exponential backoff
                 Dedicated: free slot for reuse
```

## Unstable ACP features

Build with `--features unstable` to enable support for unstable ACP methods:

- `close_session` — explicit session close with Dedicated slot cleanup
- `fork_session` — fork a session, registering the new session with a fresh external ID
- `resume_session` — resume with transparent eviction reload
- `set_session_model` — forwarded to backend

```sh
cargo build --release --features unstable
```

## Development

```sh
cargo test          # 58 tests (unit + integration)
cargo build         # standard build
cargo build --features unstable  # with unstable ACP methods
```

## License

Apache-2.0
