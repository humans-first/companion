# Companion

A multi-channel agentic companion platform. Connect AI companions to users across Telegram, Slack, WhatsApp, desktop apps, and more — all through the open [Agent Client Protocol (ACP)](https://agentclientprotocol.org/).

## Architecture

```
                  ┌──────────┐
                  │ Telegram │
                  └────┬─────┘
                       │
┌──────────┐      ┌────▼─────────┐      ┌───────────────┐      ┌───────────┐
│  Slack   ├─ACP─►│              │      │               │      │ ACP Agent │
└──────────┘      │  ACP Gateway │─ACP─►│  ACP Process  │◄────►│ (Kiro,    │
┌──────────┐      │              │      │  (companion)  │      │  Claude,  │
│ WhatsApp ├─ACP─►│  - pooling   │      │               │      │  etc.)    │
└──────────┘      │  - sessions  │      └───────────────┘      └───────────┘
┌──────────┐      │  - Cedar     │      ┌───────────────┐
│ Desktop  ├─ACP─►│    authz     │─ACP─►│  ACP Process  │
└──────────┘      └──────────────┘      └───────────────┘
```

**Connectors** (Telegram, Slack, etc.) are ACP clients that speak the protocol directly to the **ACP Gateway**. The gateway handles session multiplexing, process pooling, and Cedar-based authorization. Backend **ACP processes** are stateless runtimes that load companion configuration and execute prompts.

This design means any ACP-compatible agent works as a backend, and any ACP client works as a connector — the gateway is a protocol-aware proxy, not a translator.

## Components

### [`gateway/acp-gateway`](gateway/acp-gateway/)

The core infrastructure component. A Rust binary that proxies ACP connections between upstream clients and a pool of backend agent processes.

- **Process pooling**: Dedicated (one process per session) or least-connections (fixed pool, load-balanced)
- **Session management**: External UUID mapping, idle eviction with transparent reload
- **Cedar authorization**: Policy-based access control on prompt requests
- **Crash recovery**: Automatic respawn (LC mode) with exponential backoff
- **Health monitoring**: `gateway/status` extension method + periodic logging

### [`connectors/telegram-acp`](connectors/telegram-acp/)

A Python package that bridges Telegram bots to any ACP-compatible agent. One ACP subprocess serves all chats, each getting its own session.

- Group chat support (responds on @mention or reply)
- Message enrichment with chat context
- OpenTelemetry instrumentation
- `pip install telegram-acp`

### [`poc-site`](poc-site/)

Landing page for Companion. Static HTML/CSS/JS, deployed to Vercel.

### [`docs`](docs/)

Deployment guides and operational documentation.

## Getting started

### Run a Telegram bot with acp-gateway

```sh
# Build the gateway
cd gateway/acp-gateway
cargo build --release

# Run telegram-acp with the gateway as the ACP backend
cd connectors/telegram-acp
uv sync

TELEGRAM_BOT_TOKEN=your-token \
telegram-acp --agent-cmd "acp-gateway --agent-cmd 'kiro-cli acp -a' --strategy dedicated"
```

### Use acp-gateway with an editor

```sh
# In Zed's settings.json:
{
  "agent": {
    "type": "custom",
    "command": "acp-gateway",
    "args": ["--agent-cmd", "kiro-cli acp -a", "--strategy", "dedicated"]
  }
}
```

## Design principles

- **ACP everywhere**: Connectors, gateway, and agents all speak the same open protocol. No proprietary internal APIs.
- **Stateless processes**: Companion processes load config and execute prompts. Session state lives in the gateway and memory layer.
- **Simple stack**: Containerized, cloud-portable, minimal managed services. No high-level AI service dependencies.
- **Open components**: Connectors and infrastructure are standalone open-source packages, not Companion-specific.

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

This writes reports under [`coverage/`](/Users/igaray/projects/companion/repo/companion/coverage) when the needed local tools are installed:
- Rust workspace coverage uses `cargo-llvm-cov`
- Harness sandbox Python coverage uses `python -m trace`
- Telegram connector coverage uses `uv` + `pytest-cov`

## License

Apache-2.0
