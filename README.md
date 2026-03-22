# Companion

Companion is an ACP-native toolkit for building agent systems that can sit behind editors, chat connectors, and custom transports.

Today the repo is centered on four infrastructure pieces:

- `gateway/acp-gateway`: a multi-session ACP proxy with pooling, routing, and optional Cedar authorization
- `harness`: a stateless ACP backend that talks to an LLM, exposes MCP-backed tools, and runs code in a hardened Python sandbox
- `acp-http-transport`: a reusable HTTP transport crate that lets ACP clients and servers speak ACP over a custom streamable HTTP interface
- `connectors/telegram-acp`: a Telegram connector that bridges chats to any ACP-speaking backend

There is also a small static landing page under `poc-site/`.

## Architecture

```text
ACP clients/connectors        ACP infrastructure                   Runtime
----------------------        ------------------                   -------
Zed / Telegram / curl  <-->   acp-gateway or direct harness  <-->  LLM provider
                              ^                                 \-> MCP tool sources
                              |
                              \-> optional acp-http-transport    \-> Python sandbox
                                  for custom HTTP ACP
```

The current shape of the system is:

- ACP is the contract between components
- the gateway is the session-aware proxy
- the harness is the backend execution layer
- the HTTP transport is reusable glue, not gateway-specific

## Where We Are

What is solid today:

- The gateway can expose ACP over `stdio` or over a custom HTTP transport.
- The harness supports ACP session lifecycle methods, session modes, per-session model selection, MCP-backed tool runtimes, tool policies, and a hardened `execute` path.
- The HTTP transport crate is reusable on both the client and server side of the ACP SDK.
- Local coverage and workspace-wide Rust testing are wired at the repo root.

What is intentionally still in progress:

- Durable off-process session and history management
- Conversation compaction and longer-term memory handling
- Streaming token-by-token LLM responses
- VM or container-grade sandbox isolation
- Automatic MCP health probing and reconnection

## Repository Guide

### [`acp-http-transport`](acp-http-transport/)

A standalone Rust crate for ACP over custom streamable HTTP. It is designed to plug into the normal ACP SDK connection constructors instead of inventing a separate protocol stack.

### [`gateway/acp-gateway`](gateway/acp-gateway/)

A Rust ACP gateway that sits between upstream clients and backend ACP agents. It handles session ID translation, backend process pooling, frontend-aware routing, idle eviction, and optional Cedar authorization.

### [`harness`](harness/)

A Rust ACP backend for local and self-hosted model setups. It layers ACP handling, session management, per-session tool runtimes, MCP integration, and a hardened Python execution path.

### [`connectors/telegram-acp`](connectors/telegram-acp/)

A Python Telegram bot bridge for ACP. One subprocess can serve many chats, with one ACP session per chat.

### [`poc-site`](poc-site/)

A small static site for the Companion project. It is separate from the ACP runtime stack.

## Quick Start

Build and test the Rust workspace:

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Run the harness directly with the local example config:

```sh
cargo run -p harness -- --config harness/examples/local.json
```

Run the gateway in front of the harness:

```sh
cargo run -p acp-gateway -- \
  --agent-cmd "cargo run -p harness -- --config /Users/igaray/projects/companion/repo/companion/harness/examples/local.json" \
  --strategy dedicated \
  --idle-timeout-secs 0
```

The example harness configs target Ollama-compatible local models at `http://localhost:11434/v1` and default to `llama3.1`.

## Coverage

Local coverage is wired at the repo root:

```sh
./scripts/coverage.sh
```

Useful variants:

```sh
./scripts/coverage.sh --rust-only
./scripts/coverage.sh --python-only
```

Reports are written under [`coverage/`](coverage/).

## License

Apache-2.0
