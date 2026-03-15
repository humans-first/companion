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

import logging

from acp import text_block
from telegram import Update
from telegram.constants import ChatAction, ChatType
from telegram.ext import (
    Application,
    CommandHandler,
    ContextTypes,
    MessageHandler,
    filters,
)

from telegram_acp.acp import ACPManager
from telegram_acp.config import get_settings

logger = logging.getLogger(__name__)

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


def _build_chat_header(chat) -> str:
    """Build a [Chat: ...] header for group context."""
    if chat.type in (ChatType.PRIVATE,):
        return ""
    title = chat.title or "Untitled"
    return f'[Chat: "{title}" | {chat.type}]\n'


def _build_reply_context(reply_msg, bot_id: int) -> str:
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
    if not allowed(update.effective_chat.id):
        return
    await update.message.reply_text(
        f"Hey {update.effective_user.first_name}. Send me a message."
    )


async def cmd_reset(update: Update, _ctx: ContextTypes.DEFAULT_TYPE) -> None:
    if not allowed(update.effective_chat.id):
        return
    await _acp.reset(update.effective_chat.id)
    await update.message.reply_text("Session reset.")


async def cmd_chatid(update: Update, _ctx: ContextTypes.DEFAULT_TYPE) -> None:
    cid = update.effective_chat.id
    await update.message.reply_text(f"Chat ID: `{cid}`", parse_mode="Markdown")


async def _track_message(update: Update, _ctx: ContextTypes.DEFAULT_TYPE) -> None:
    """Track the latest message ID per chat (runs on every message via group=-1)."""
    _last_seen_msg[update.effective_chat.id] = update.message.message_id


async def _respond(update: Update, ctx: ContextTypes.DEFAULT_TYPE, text: str) -> None:
    """Build enriched prompt, send to ACP, reply with the result."""
    chat = update.effective_chat
    chat_id = chat.id
    user = update.effective_user
    display_name = user.full_name or user.username or str(user.id)

    await ctx.bot.send_chat_action(chat_id, ChatAction.TYPING)

    # Build enriched prompt
    header = _build_chat_header(chat)
    reply_ctx = _build_reply_context(update.message.reply_to_message, ctx.bot.id)
    prompt = f"{header}{reply_ctx}[{display_name}]: {text}"

    try:
        full = ""
        async for chunk in _acp.prompt(chat_id, [text_block(prompt)]):
            full += chunk

        if not full.strip():
            return

        parts = [full[i : i + 4096] for i in range(0, len(full), 4096)]

        # Reply with threading only if other messages arrived since the trigger
        is_latest = _last_seen_msg.get(chat_id) == update.message.message_id
        if is_latest:
            await ctx.bot.send_message(chat_id, parts[0])
            for part in parts[1:]:
                await ctx.bot.send_message(chat_id, part)
        else:
            await update.message.reply_text(parts[0])
            for part in parts[1:]:
                await update.message.reply_text(part)

    except Exception:
        logger.exception("Error handling message from chat %d", chat_id)
        await update.message.reply_text("Something went wrong. Try /reset and send again.")
        await _acp.reset(chat_id)


async def handle_message(update: Update, ctx: ContextTypes.DEFAULT_TYPE) -> None:
    logger.debug(
        "incoming message chat=%s type=%s text=%r",
        update.effective_chat.id,
        update.effective_chat.type,
        (update.message.text or "")[:120],
    )

    if not allowed(update.effective_chat.id):
        return

    text = update.message.text or ""
    chat_type = update.effective_chat.type

    # In groups: respond when @mentioned OR when replying to the bot's own message
    if chat_type in (ChatType.GROUP, ChatType.SUPERGROUP):
        bot_username = ctx.bot.username
        mentioned = f"@{bot_username}".lower() in text.lower()
        replied_to_bot = (
            update.message.reply_to_message is not None
            and update.message.reply_to_message.from_user is not None
            and update.message.reply_to_message.from_user.id == ctx.bot.id
        )
        if not mentioned and not replied_to_bot:
            return
        text = strip_mention(text, bot_username)
        if not text:
            return

    await _respond(update, ctx, text)


# ------------------------------------------------------------------ #
# Lifecycle                                                            #
# ------------------------------------------------------------------ #


async def post_init(app: Application) -> None:
    await _acp.start()
    logger.info(
        "Bot started. Allowed chats: %s",
        sorted(_allowed) if _allowed else "open (no allowlist)",
    )


async def post_shutdown(app: Application) -> None:
    await _acp.stop()


def main() -> None:
    global _acp, _allowed

    settings = get_settings()

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
    app.add_handler(
        MessageHandler(filters.TEXT & ~filters.COMMAND, _track_message), group=-1
    )
    app.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, handle_message))

    app.run_polling(drop_pending_updates=True)


if __name__ == "__main__":
    main()
