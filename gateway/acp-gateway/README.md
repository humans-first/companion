# acp-gateway

`acp-gateway` is an ACP proxy that sits between one or more upstream clients and a pool of backend ACP agent processes.

It exists to solve the infrastructure concerns that individual ACP backends should not have to own:

- external session IDs and backend session remapping
- process pooling and placement
- prompt serialization and session routing
- optional Cedar authorization at the gateway edge
- exposing ACP over `stdio` or over the repo's reusable HTTP transport

## Current Role In The Stack

```text
ACP client or connector  <-->  acp-gateway  <-->  backend ACP agent(s)
Zed / Telegram / curl           |                    harness / other agents
                                \-> session routing
                                \-> pooling
                                \-> Cedar authz
```

In HTTP mode, the gateway runs as a shared multi-frontend service. Each frontend gets its own ACP connection, while the backend pool is shared.

## What It Does Today

- Exposes ACP over `stdio` or a custom HTTP transport
- Supports `dedicated` and `least-connections` backend placement strategies
- Rewrites session IDs so upstream clients do not see backend-internal IDs
- Tracks the last sender for each session and pins prompt-time callbacks to the active prompt sender
- Supports idle eviction and transparent reload when the backend supports it
- Optionally authorizes prompt requests with Cedar policies
- Exposes a `gateway/status` extension method for pool inspection

## Quick Start

Build it:

```sh
cargo build --release
```

Run it over `stdio` in front of the harness:

```sh
./target/release/acp-gateway \
  --agent-cmd "/Users/igaray/projects/companion/repo/companion/harness/target/release/harness --config /Users/igaray/projects/companion/repo/companion/harness/examples/local.json" \
  --strategy dedicated \
  --idle-timeout-secs 0
```

Run it over HTTP:

```sh
./target/release/acp-gateway \
  --transport http \
  --http-bind 127.0.0.1:8787 \
  --agent-cmd "/Users/igaray/projects/companion/repo/companion/harness/target/release/harness --config /Users/igaray/projects/companion/repo/companion/harness/examples/local.json" \
  --strategy dedicated \
  --idle-timeout-secs 0
```

The HTTP surface is provided by [`acp-http-transport`](../../acp-http-transport/).

Enable HTTPS on the gateway transport by adding:

```sh
  --http-tls-cert /path/to/cert.pem \
  --http-tls-key /path/to/key.pem
```

## Strategies

### `dedicated`

One backend process per session, spawned on demand.

Use this when you want:

- isolation between sessions
- simpler backend reasoning
- a better fit for agents that keep meaningful in-process state

### `least-connections`

A fixed pool of backend processes spawned up front. New sessions are assigned to the least-loaded backend.

Use this when you want:

- lower process churn
- a smaller fixed backend fleet
- a better fit for lightweight shared backends

## Cedar Authorization

Prompt requests can be gated with Cedar:

```sh
./target/release/acp-gateway \
  --agent-cmd "..." \
  --policy-dir ./policies \
  --schema-file ./policies/schema.cedarschema
```

The gateway reads `principal`, `action`, and `resource` from ACP request `_meta`.

Example:

```json
{
  "_meta": {
    "principal": "User::\"alice\"",
    "action": "Action::\"prompt\"",
    "resource": "Agent::\"companion\""
  }
}
```

## HTTP Mode

The gateway can act as an ACP server over HTTP by using the transport crate in this repo. In that mode it exposes ACP through:

- `POST /v1/acp/connections`
- `GET /v1/acp/connections/{id}/stream`
- `POST /v1/acp/connections/{id}/messages`
- `DELETE /v1/acp/connections/{id}`

This is a custom transport layer for ACP, not a replacement for ACP itself.

## Important Limits

- Session mappings are in memory inside the gateway process.
- Transparent reload only works as well as the backend's `load_session` and `resume_session` support.
- The current harness binary uses an in-memory session store by default, so if you want stable continuity through restarts or eviction, that storage layer still needs to arrive first.
- Unscoped backend extension callbacks are ambiguous when multiple frontends are connected, so those flows are only safe when one frontend is attached.
- HTTP mode can now terminate TLS directly, but it still does not provide gateway-level authentication by itself.

## Unstable ACP Methods

Build with `--features unstable` if you want the gateway to forward unstable ACP session methods such as close, fork, resume, and set-model:

```sh
cargo build --release --features unstable
```

## Development

```sh
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## License

Apache-2.0
