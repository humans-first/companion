"""
Telegram-to-ACP connector.

A generic bridge that connects any Telegram bot to any ACP-compatible agent.

Commands:
  /start   - greeting
  /reset   - reset ACP session for this chat
  /chatid  - print the current chat ID

In groups: responds only when @mentioned or when replying to the bot's message.
In private chats: responds to every message.
"""

from __future__ import annotations

import argparse
import logging
import time

from acp import text_block
from opentelemetry import metrics, trace
from telegram import Chat, Message, Update, User
from telegram.constants import ChatAction, ChatType
from telegram.ext import (
    Application,
    CommandHandler,
    ContextTypes,
    MessageHandler,
    filters,
)

from telegram_acp import __version__
from telegram_acp.acp import ACPManager
from telegram_acp.config import Settings

logger = logging.getLogger(__name__)
tracer = trace.get_tracer("telegram_acp.main")
meter = metrics.get_meter("telegram_acp.main")

messages_received = meter.create_counter(
    "telegram.messages.received",
    description="Total messages received from Telegram",
)
responses_sent = meter.create_counter(
    "telegram.responses.sent",
    description="Total responses sent back to Telegram",
)
response_failures = meter.create_counter(
    "telegram.responses.failures",
    description="Total failed response attempts",
)
response_duration = meter.create_histogram(
    "telegram.response.duration",
    unit="s",
    description="End-to-end time from receiving a message to sending the response",
)

# Initialized in main() after settings are loaded
_acp: ACPManager | None = None
_allowed: set[int] = set()

# Track the last seen message ID per chat for reply-vs-send decisions
_last_seen_msg: dict[int, int] = {}


def allowed(chat_id: int) -> bool:
    return not _allowed or chat_id in _allowed


def strip_mention(text: str, bot_username: str) -> str:
    mention = f"@{bot_username}"
    return text.replace(mention, "").strip()


def _build_chat_header(chat: Chat) -> str:
    """Build a [Chat: ...] header for group context."""
    if chat.type == ChatType.PRIVATE:
        return ""
    title = chat.title or "Untitled"
    return f'[Chat: "{title}" | {chat.type}]\n'


def _build_reply_context(reply_msg: Message | None, bot_id: int) -> str:
    """Build a [Replying to ...] line if the message is a reply."""
    if not reply_msg or not reply_msg.text:
        return ""
    author = reply_msg.from_user
    if author and author.id == bot_id:
        name = "the assistant"
    elif author:
        name = author.full_name or author.username or str(author.id)
    else:
        name = "unknown"
    return f'[Replying to {name}: "{reply_msg.text}"]\n'


# ------------------------------------------------------------------ #
# Handlers                                                             #
# ------------------------------------------------------------------ #


async def cmd_start(update: Update, _ctx: ContextTypes.DEFAULT_TYPE) -> None:
    chat = _get_chat(update)
    msg = _get_message(update)
    user = _get_user(update)
    if not allowed(chat.id):
        return
    await msg.reply_text(f"Hey {user.first_name}. Send me a message.")


async def cmd_reset(update: Update, _ctx: ContextTypes.DEFAULT_TYPE) -> None:
    chat = _get_chat(update)
    msg = _get_message(update)
    if not allowed(chat.id):
        return
    assert _acp is not None
    await _acp.reset(chat.id)
    await msg.reply_text("Session reset.")


async def cmd_chatid(update: Update, _ctx: ContextTypes.DEFAULT_TYPE) -> None:
    chat = _get_chat(update)
    msg = _get_message(update)
    await msg.reply_text(f"Chat ID: `{chat.id}`", parse_mode="Markdown")


async def _track_message(update: Update, _ctx: ContextTypes.DEFAULT_TYPE) -> None:
    """Track the latest message ID per chat (runs on every message via group=-1)."""
    chat = _get_chat(update)
    msg = _get_message(update)
    _last_seen_msg[chat.id] = msg.message_id


async def _respond(update: Update, ctx: ContextTypes.DEFAULT_TYPE, text: str) -> None:
    """Build enriched prompt, send to ACP, reply with the result."""
    assert _acp is not None
    chat = _get_chat(update)
    msg = _get_message(update)
    user = _get_user(update)
    chat_id = chat.id
    display_name = user.full_name or user.username or str(user.id)

    with tracer.start_as_current_span(
        "telegram.respond",
        attributes={
            "chat.id": chat_id,
            "chat.type": str(chat.type),
            "user.display_name": display_name,
        },
    ) as span:
        t0 = time.monotonic()
        attrs = {"chat.type": str(chat.type)}
        await ctx.bot.send_chat_action(chat_id, ChatAction.TYPING)

        # Build enriched prompt
        header = _build_chat_header(chat)
        reply_ctx = _build_reply_context(msg.reply_to_message, ctx.bot.id)
        prompt = f"{header}{reply_ctx}[{display_name}]: {text}"

        # Build ACP _meta with principal and context for downstream auth
        meta = {
            "principal": {
                "platform": "telegram",
                "user_id": user.id,
                "display_name": display_name,
                "username": user.username,
            },
            "context": {
                "chat_id": chat_id,
                "chat_type": str(chat.type),
                "chat_title": chat.title,
            },
        }

        try:
            full = ""
            async for chunk in _acp.prompt(chat_id, [text_block(prompt)], meta=meta):
                full += chunk

            if not full.strip():
                span.set_attribute("telegram.empty_response", True)
                return

            parts = [full[i : i + 4096] for i in range(0, len(full), 4096)]
            span.set_attribute("telegram.response_length", len(full))
            span.set_attribute("telegram.response_parts", len(parts))

            # Reply with threading only if other messages arrived since the trigger
            is_latest = _last_seen_msg.get(chat_id) == msg.message_id
            span.set_attribute("telegram.threaded", not is_latest)
            if is_latest:
                await ctx.bot.send_message(chat_id, parts[0], parse_mode="MarkdownV2")
                for part in parts[1:]:
                    await ctx.bot.send_message(chat_id, part, parse_mode="MarkdownV2")
            else:
                await msg.reply_text(parts[0], parse_mode="MarkdownV2")
                for part in parts[1:]:
                    await msg.reply_text(part, parse_mode="MarkdownV2")

            responses_sent.add(1, attrs)

        except Exception as exc:
            span.set_status(trace.StatusCode.ERROR)
            span.record_exception(exc)
            response_failures.add(1, attrs)
            logger.exception("Error handling message from chat %d", chat_id)
            await msg.reply_text("Something went wrong. Try /reset and send again.")
            await _acp.reset(chat_id)

        finally:
            response_duration.record(time.monotonic() - t0, attrs)


async def handle_message(update: Update, ctx: ContextTypes.DEFAULT_TYPE) -> None:
    chat = _get_chat(update)
    msg = _get_message(update)

    with tracer.start_as_current_span(
        "telegram.handle_message",
        attributes={
            "chat.id": chat.id,
            "chat.type": str(chat.type),
        },
    ) as span:
        logger.debug(
            "incoming message chat=%s type=%s text=%r",
            chat.id,
            chat.type,
            (msg.text or "")[:120],
        )

        messages_received.add(1, {"chat.type": str(chat.type)})

        if not allowed(chat.id):
            span.set_attribute("telegram.action", "blocked_by_allowlist")
            return

        text = msg.text or ""
        chat_type = chat.type

        # In groups: respond when @mentioned OR when replying to the bot's own message
        if chat_type in (ChatType.GROUP, ChatType.SUPERGROUP):
            bot_username = ctx.bot.username
            mentioned = f"@{bot_username}".lower() in text.lower()
            replied_to_bot = (
                msg.reply_to_message is not None
                and msg.reply_to_message.from_user is not None
                and msg.reply_to_message.from_user.id == ctx.bot.id
            )
            if not mentioned and not replied_to_bot:
                span.set_attribute("telegram.action", "ignored_group_message")
                return
            text = strip_mention(text, bot_username)
            if not text:
                span.set_attribute("telegram.action", "empty_after_strip")
                return
            span.set_attribute("telegram.action", "group_respond")
        else:
            span.set_attribute("telegram.action", "private_respond")

        await _respond(update, ctx, text)


# ------------------------------------------------------------------ #
# Helpers                                                              #
# ------------------------------------------------------------------ #


def _get_chat(update: Update) -> Chat:
    assert update.effective_chat is not None
    return update.effective_chat


def _get_message(update: Update) -> Message:
    assert update.message is not None
    return update.message


def _get_user(update: Update) -> User:
    assert update.effective_user is not None
    return update.effective_user


# ------------------------------------------------------------------ #
# Lifecycle                                                            #
# ------------------------------------------------------------------ #


async def post_init(_app: Application) -> None:  # type: ignore[type-arg]
    assert _acp is not None
    await _acp.start()
    logger.info(
        "Bot started. Allowed chats: %s",
        sorted(_allowed) if _allowed else "open (no allowlist)",
    )


async def post_shutdown(_app: Application) -> None:  # type: ignore[type-arg]
    assert _acp is not None
    await _acp.stop()


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="telegram-acp",
        description="Bridge a Telegram bot to an ACP-compatible agent.",
    )
    parser.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    parser.add_argument("--token", help="Telegram bot token (env: TELEGRAM_TOKEN)")
    parser.add_argument("--acp-cmd", help="ACP server command (env: ACP_SERVER_CMD)")
    parser.add_argument("--session-mode", help="ACP session mode (env: ACP_SESSION_MODE)")
    parser.add_argument("--allowed-chats", help="Comma-separated chat IDs (env: ALLOWED_CHATS)")
    parser.add_argument(
        "--debug", action="store_true", default=None, help="Log raw ACP messages (env: DEBUG_ACP)"
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    global _acp, _allowed

    args = _parse_args(argv)

    # CLI flags override env vars / .env
    overrides: dict[str, str | bool] = {}
    if args.token is not None:
        overrides["telegram_token"] = args.token
    if args.acp_cmd is not None:
        overrides["acp_server_cmd"] = args.acp_cmd
    if args.session_mode is not None:
        overrides["acp_session_mode"] = args.session_mode
    if args.allowed_chats is not None:
        overrides["allowed_chats"] = args.allowed_chats
    if args.debug is not None:
        overrides["debug_acp"] = args.debug

    settings = Settings(**overrides)  # type: ignore[arg-type]

    logging.basicConfig(
        level=logging.DEBUG if settings.debug_acp else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    _acp = ACPManager(
        cmd=settings.acp_server_argv,
        session_mode=settings.acp_session_mode,
        debug=settings.debug_acp,
    )
    _allowed = settings.allowed_chat_ids

    app = (
        Application.builder()
        .token(settings.telegram_token)
        .post_init(post_init)
        .post_shutdown(post_shutdown)
        .build()
    )

    app.add_handler(CommandHandler("start", cmd_start))
    app.add_handler(CommandHandler("reset", cmd_reset))
    app.add_handler(CommandHandler("chatid", cmd_chatid))
    app.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, _track_message), group=-1)
    app.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, handle_message))

    app.run_polling(drop_pending_updates=True)


if __name__ == "__main__":
    main()
