# telegram-acp

`telegram-acp` is a Telegram-to-ACP connector. It lets one Telegram bot talk to any ACP-compatible backend, including the gateway and harness in this repo.

## What It Does

- Spawns one ACP subprocess and reuses it across chats
- Creates one ACP session per Telegram chat
- Serializes prompts within a chat while allowing different chats to run concurrently
- Responds in groups only when mentioned or when replying to the bot
- Enriches prompts with chat and reply context before sending them downstream
- Emits OpenTelemetry traces and metrics when an SDK is installed

The simplest topology is:

```text
Telegram bot  <-->  telegram-acp  <-->  ACP backend
                                    acp-gateway or harness
```

## Install

```sh
pip install telegram-acp
```

Or from source:

```sh
uv sync
```

## Configuration

Settings resolve in this order:

1. CLI flags
2. Environment variables
3. `.env` in the working directory

| Setting | CLI flag | Env var | Default |
|---|---|---|---|
| Telegram token | `--token` | `TELEGRAM_TOKEN` | required |
| ACP server command | `--acp-cmd` | `ACP_SERVER_CMD` | `kiro cli acp` |
| ACP session mode | `--session-mode` | `ACP_SESSION_MODE` | empty |
| Allowed chats | `--allowed-chats` | `ALLOWED_CHATS` | empty |
| Debug ACP logging | `--debug` | `DEBUG_ACP` | `false` |

One important detail: `ACP_SERVER_CMD` is treated as a space-separated argv list, not as a shell command line. If your backend launch needs nested quoting, wrap it in a small script and point `ACP_SERVER_CMD` at that script instead.

## Usage

Run directly against the harness:

```sh
TELEGRAM_TOKEN=123:abc \
telegram-acp --acp-cmd "/Users/igaray/projects/companion/repo/companion/harness/target/release/harness --config /Users/igaray/projects/companion/repo/companion/harness/examples/local.json"
```

Use `/chatid` in any chat to discover its ID. Group IDs are negative numbers.

## Chat Behavior

In private chats, every message is forwarded.

In groups and supergroups, the bot only responds when:

- it is mentioned, or
- the user replies to one of the bot's messages

Before a prompt is sent downstream, the connector adds lightweight context such as:

- chat title and chat type
- reply context
- the Telegram user's display name

It also attaches ACP `_meta` describing the Telegram user and chat context.

That metadata is useful downstream, but it is not the same shape as the Cedar-oriented `principal` / `action` / `resource` tuple that `acp-gateway` expects for prompt authorization. If you want to enforce gateway Cedar policies directly from Telegram traffic, you still need a metadata-mapping step.

## Bot Commands

| Command | Description |
|---|---|
| `/start` | Greeting |
| `/reset` | Reset the ACP session for the current chat |
| `/chatid` | Show the current chat ID |

## Observability

The connector uses OpenTelemetry APIs for traces and metrics. Without an SDK installed, instrumentation is effectively no-op.

Typical spans include:

- `telegram.handle_message`
- `telegram.respond`
- `acp.start`
- `acp.new_session`
- `acp.prompt`
- `acp.stop`

## Development

```sh
uv sync
uv run ruff check src/ tests/
uv run mypy
uv run pytest --cov
```

## License

Apache-2.0
