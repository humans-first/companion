# telegram-acp

A generic Telegram-to-ACP connector. Bridge any Telegram bot to any [ACP](https://agentclientprotocol.org/)-compatible agent backend.

## Features

- **Single process, multiple sessions** -- one ACP subprocess serves all chats, each getting its own session
- **Group support** -- responds when @mentioned or replied to; ignores other messages
- **Message enrichment** -- sends chat context (title, type, reply-to) alongside user text
- **Smart threading** -- replies inline only when newer messages have arrived
- **OpenTelemetry instrumentation** -- traces and metrics out of the box (no-op without an SDK)

## Requirements

- Python 3.11+
- A Telegram bot token (from [@BotFather](https://t.me/BotFather))
- An ACP-compatible agent (any command that speaks the [Agent Client Protocol](https://agentclientprotocol.org/))

## Install

```bash
pip install telegram-acp
```

Or from source:

```bash
cd connectors/telegram-acp
uv sync
```

## Configuration

Settings are resolved in this order (highest priority first):

1. CLI flags
2. Environment variables
3. `.env` file in the working directory

| Setting | CLI flag | Env var | Default | Description |
|---|---|---|---|---|
| Telegram token | `--token` | `TELEGRAM_TOKEN` | *(required)* | Bot token from @BotFather |
| ACP server command | `--acp-cmd` | `ACP_SERVER_CMD` | `kiro cli acp` | Command to spawn the ACP agent subprocess |
| Session mode | `--session-mode` | `ACP_SESSION_MODE` | `""` | ACP session mode to set after creation (e.g. `chat`) |
| Allowed chats | `--allowed-chats` | `ALLOWED_CHATS` | `""` | Comma-separated chat IDs; empty means all chats |
| Debug logging | `--debug` | `DEBUG_ACP` | `false` | Log raw ACP messages |

Use `/chatid` in any Telegram chat to discover its ID (group IDs are negative numbers).

### Examples

Using a `.env` file:

```bash
cp .env.example .env
# edit .env, set TELEGRAM_TOKEN at minimum
telegram-acp
```

Using environment variables:

```bash
TELEGRAM_TOKEN=123:abc telegram-acp
```

Using CLI flags:

```bash
telegram-acp --token 123:abc --acp-cmd "my-agent serve" --debug
```

Mixing sources (CLI overrides env vars which override `.env`):

```bash
TELEGRAM_TOKEN=123:abc telegram-acp --debug --allowed-chats "111,-222"
```

## Usage

```bash
telegram-acp
# or
uv run telegram-acp
```

```
$ telegram-acp --help
usage: telegram-acp [-h] [--version] [--token TOKEN] [--acp-cmd ACP_CMD]
                    [--session-mode SESSION_MODE]
                    [--allowed-chats ALLOWED_CHATS] [--debug]

Bridge a Telegram bot to an ACP-compatible agent.
```

## Bot commands

| Command  | Description                         |
|----------|-------------------------------------|
| `/start` | Greeting                            |
| `/reset` | Reset the ACP session for this chat |
| `/chatid`| Print the current chat ID           |

## How it works

The connector spawns a single ACP subprocess and creates a separate session for each Telegram chat. Messages within a chat are serialized (one prompt at a time), but different chats can prompt concurrently.

In **private chats**, every message is forwarded to the agent.

In **groups**, the bot only responds when:
- It is @mentioned in a message, or
- A user replies to one of the bot's messages

Before sending to the agent, the connector enriches the message with context:

```
[Chat: "Book Club" | supergroup]
[Replying to Bob: "what about chapter 3?"]
[Alice]: I loved the twist
```

Responses longer than 4096 characters (Telegram's limit) are automatically split into multiple messages.

## Observability

The connector is instrumented with [OpenTelemetry](https://opentelemetry.io/) traces and metrics using the `opentelemetry-api` package. Without an OTel SDK installed, all instrumentation is no-op (zero overhead).

To enable observability, install the SDK and an exporter:

```bash
pip install opentelemetry-sdk opentelemetry-exporter-otlp
```

Then run with auto-instrumentation:

```bash
OTEL_SERVICE_NAME=telegram-acp \
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
opentelemetry-instrument telegram-acp
```

### Traces

| Span | Key attributes |
|------|----------------|
| `telegram.handle_message` | `chat.id`, `chat.type`, `telegram.action` |
| `telegram.respond` | `chat.id`, `chat.type`, `user.display_name`, `telegram.response_length`, `telegram.threaded` |
| `acp.start` | `acp.cmd`, `acp.pid` |
| `acp.new_session` | `chat.id`, `acp.session_id` |
| `acp.prompt` | `chat.id`, `acp.session_id`, `acp.chunk_count` |
| `acp.stop` | -- |

### Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `telegram.messages.received` | Counter | Messages received, by `chat.type` |
| `telegram.responses.sent` | Counter | Successful responses sent |
| `telegram.responses.failures` | Counter | Failed response attempts |
| `telegram.response.duration` | Histogram (s) | End-to-end latency per response |
| `acp.prompt.duration` | Histogram (s) | ACP prompt round-trip time |
| `acp.prompt.successes` | Counter | Successful ACP prompts |
| `acp.prompt.failures` | Counter | Failed ACP prompts |
| `acp.sessions.active` | UpDownCounter | Currently active sessions |
| `acp.process.respawns` | Counter | ACP process respawn count |

## Development

```bash
uv sync
uv run ruff check src/ tests/    # lint
uv run mypy                       # type check
uv run pytest --cov               # test with coverage
```

## License

Apache-2.0
