# telegram-acp

A generic Telegram-to-ACP connector. Bridge any Telegram bot to any [ACP](https://agentclientprotocol.org/)-compatible agent backend.

## Features

- **Single process, multiple sessions** — one ACP subprocess serves all chats, each getting its own session
- **Group support** — responds when @mentioned or replied to; ignores other messages
- **Message enrichment** — sends chat context (title, type, reply-to) alongside user text
- **Smart threading** — replies inline only when newer messages have arrived

## Install

```bash
pip install telegram-acp
```

Or from source:

```bash
cd connectors/telegram-acp
uv sync
```

## Configure

Copy `.env.example` to `.env` and fill in your values:

```
TELEGRAM_TOKEN=your-bot-token    # Required. Get one from @BotFather
ACP_SERVER_CMD=kiro cli acp      # Command to spawn the ACP agent
ACP_SESSION_MODE=                 # Optional session mode (e.g. "chat")
ALLOWED_CHATS=                    # Comma-separated chat IDs (empty = all)
DEBUG_ACP=false                   # Log raw ACP messages
```

Use `/chatid` in any chat to get its ID (group IDs are negative).

## Run

```bash
telegram-acp
```

Or with uv:

```bash
uv run telegram-acp
```

## Bot commands

| Command    | Description                        |
|------------|------------------------------------|
| `/start`   | Greeting                           |
| `/reset`   | Reset the ACP session for this chat |
| `/chatid`  | Print the current chat ID          |

## License

Apache-2.0
