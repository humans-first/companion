from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from telegram.constants import ChatType

from telegram_acp import main

# ------------------------------------------------------------------ #
# Pure functions                                                       #
# ------------------------------------------------------------------ #


class TestAllowed:
    def test_empty_allowlist_permits_all(self):
        main._allowed = set()
        assert main.allowed(123) is True
        assert main.allowed(-456) is True

    def test_allowlist_filters(self):
        main._allowed = {123, -456}
        assert main.allowed(123) is True
        assert main.allowed(-456) is True
        assert main.allowed(789) is False

    def test_single_chat_allowlist(self):
        main._allowed = {42}
        assert main.allowed(42) is True
        assert main.allowed(43) is False

    def teardown_method(self):
        main._allowed = set()


class TestStripMention:
    def test_strips_mention(self):
        assert main.strip_mention("@mybot hello", "mybot") == "hello"

    def test_strips_multiple_mentions(self):
        assert main.strip_mention("@mybot hi @mybot", "mybot") == "hi"

    def test_no_mention(self):
        assert main.strip_mention("hello world", "mybot") == "hello world"

    def test_mention_only(self):
        assert main.strip_mention("@mybot", "mybot") == ""

    def test_mention_with_surrounding_spaces(self):
        assert main.strip_mention("  @mybot  hi  ", "mybot") == "hi"

    def test_mention_in_middle(self):
        assert main.strip_mention("hey @mybot what's up", "mybot") == "hey  what's up"

    def test_different_bot_name_not_stripped(self):
        assert main.strip_mention("@otherbot hello", "mybot") == "@otherbot hello"


class TestBuildChatHeader:
    def test_private_chat_returns_empty(self):
        chat = SimpleNamespace(type=ChatType.PRIVATE, title=None)
        assert main._build_chat_header(chat) == ""

    def test_group_chat(self):
        chat = SimpleNamespace(type=ChatType.GROUP, title="Book Club")
        assert main._build_chat_header(chat) == '[Chat: "Book Club" | group]\n'

    def test_supergroup_chat(self):
        chat = SimpleNamespace(type=ChatType.SUPERGROUP, title="Dev Team")
        assert main._build_chat_header(chat) == '[Chat: "Dev Team" | supergroup]\n'

    def test_group_without_title(self):
        chat = SimpleNamespace(type=ChatType.GROUP, title=None)
        assert main._build_chat_header(chat) == '[Chat: "Untitled" | group]\n'

    def test_channel_chat(self):
        chat = SimpleNamespace(type=ChatType.CHANNEL, title="News")
        assert main._build_chat_header(chat) == '[Chat: "News" | channel]\n'


class TestBuildReplyContext:
    def test_no_reply(self):
        assert main._build_reply_context(None, bot_id=100) == ""

    def test_reply_no_text(self):
        reply = SimpleNamespace(text=None, from_user=None)
        assert main._build_reply_context(reply, bot_id=100) == ""

    def test_reply_empty_text(self):
        reply = SimpleNamespace(text="", from_user=None)
        assert main._build_reply_context(reply, bot_id=100) == ""

    def test_reply_to_bot(self):
        author = SimpleNamespace(id=100, full_name="Bot", username="bot")
        reply = SimpleNamespace(text="previous message", from_user=author)
        assert main._build_reply_context(reply, bot_id=100) == (
            '[Replying to the assistant: "previous message"]\n'
        )

    def test_reply_to_user(self):
        author = SimpleNamespace(id=200, full_name="Alice", username="alice")
        reply = SimpleNamespace(text="what about chapter 3?", from_user=author)
        assert main._build_reply_context(reply, bot_id=100) == (
            '[Replying to Alice: "what about chapter 3?"]\n'
        )

    def test_reply_to_user_username_fallback(self):
        author = SimpleNamespace(id=200, full_name=None, username="bob42")
        reply = SimpleNamespace(text="hey", from_user=author)
        assert main._build_reply_context(reply, bot_id=100) == (
            '[Replying to bob42: "hey"]\n'
        )

    def test_reply_to_user_id_fallback(self):
        author = SimpleNamespace(id=200, full_name=None, username=None)
        reply = SimpleNamespace(text="hey", from_user=author)
        assert main._build_reply_context(reply, bot_id=100) == (
            '[Replying to 200: "hey"]\n'
        )

    def test_reply_unknown_author(self):
        reply = SimpleNamespace(text="mystery", from_user=None)
        assert main._build_reply_context(reply, bot_id=100) == (
            '[Replying to unknown: "mystery"]\n'
        )


# ------------------------------------------------------------------ #
# Telegram mock helpers                                                #
# ------------------------------------------------------------------ #


def _make_update(
    chat_id=100,
    chat_type=ChatType.PRIVATE,
    text="hello",
    message_id=1,
    user_first_name="Alice",
    user_full_name="Alice Smith",
    user_username="alice",
    user_id=200,
    chat_title=None,
    reply_to_message=None,
):
    msg = MagicMock()
    msg.text = text
    msg.message_id = message_id
    msg.reply_text = AsyncMock()
    msg.reply_to_message = reply_to_message

    chat = MagicMock()
    chat.id = chat_id
    chat.type = chat_type
    chat.title = chat_title

    user = MagicMock()
    user.id = user_id
    user.first_name = user_first_name
    user.full_name = user_full_name
    user.username = user_username

    update = MagicMock()
    update.effective_chat = chat
    update.message = msg
    update.effective_user = user
    return update


def _make_ctx(bot_id=999, bot_username="testbot"):
    ctx = MagicMock()
    ctx.bot.id = bot_id
    ctx.bot.username = bot_username
    ctx.bot.send_chat_action = AsyncMock()
    ctx.bot.send_message = AsyncMock()
    return ctx


# ------------------------------------------------------------------ #
# Command handlers — happy paths                                      #
# ------------------------------------------------------------------ #


class TestCmdStart:
    @pytest.mark.asyncio
    async def test_sends_greeting(self):
        update = _make_update()
        ctx = _make_ctx()
        main._allowed = set()
        await main.cmd_start(update, ctx)
        update.message.reply_text.assert_awaited_once_with(
            "Hey Alice. Send me a message."
        )

    @pytest.mark.asyncio
    async def test_blocked_by_allowlist(self):
        update = _make_update(chat_id=100)
        ctx = _make_ctx()
        main._allowed = {999}
        await main.cmd_start(update, ctx)
        update.message.reply_text.assert_not_awaited()

    def teardown_method(self):
        main._allowed = set()


class TestCmdReset:
    @pytest.mark.asyncio
    async def test_resets_session(self):
        mock_acp = AsyncMock()
        main._acp = mock_acp
        main._allowed = set()
        update = _make_update(chat_id=42)
        ctx = _make_ctx()
        await main.cmd_reset(update, ctx)
        mock_acp.reset.assert_awaited_once_with(42)
        update.message.reply_text.assert_awaited_once_with("Session reset.")

    @pytest.mark.asyncio
    async def test_blocked_by_allowlist(self):
        mock_acp = AsyncMock()
        main._acp = mock_acp
        main._allowed = {999}
        update = _make_update(chat_id=42)
        ctx = _make_ctx()
        await main.cmd_reset(update, ctx)
        mock_acp.reset.assert_not_awaited()

    def teardown_method(self):
        main._acp = None
        main._allowed = set()


class TestCmdChatid:
    @pytest.mark.asyncio
    async def test_prints_chat_id(self):
        update = _make_update(chat_id=-12345)
        ctx = _make_ctx()
        await main.cmd_chatid(update, ctx)
        update.message.reply_text.assert_awaited_once_with(
            "Chat ID: `-12345`", parse_mode="Markdown"
        )

    @pytest.mark.asyncio
    async def test_prints_positive_chat_id(self):
        update = _make_update(chat_id=42)
        ctx = _make_ctx()
        await main.cmd_chatid(update, ctx)
        update.message.reply_text.assert_awaited_once_with(
            "Chat ID: `42`", parse_mode="Markdown"
        )


class TestTrackMessage:
    @pytest.mark.asyncio
    async def test_tracks_message_id(self):
        main._last_seen_msg.clear()
        update = _make_update(chat_id=100, message_id=42)
        ctx = _make_ctx()
        await main._track_message(update, ctx)
        assert main._last_seen_msg[100] == 42

    @pytest.mark.asyncio
    async def test_tracks_latest_message(self):
        main._last_seen_msg.clear()
        ctx = _make_ctx()
        await main._track_message(_make_update(chat_id=100, message_id=1), ctx)
        await main._track_message(_make_update(chat_id=100, message_id=5), ctx)
        await main._track_message(_make_update(chat_id=100, message_id=3), ctx)
        assert main._last_seen_msg[100] == 3  # last call wins

    @pytest.mark.asyncio
    async def test_tracks_per_chat(self):
        main._last_seen_msg.clear()
        ctx = _make_ctx()
        await main._track_message(_make_update(chat_id=100, message_id=1), ctx)
        await main._track_message(_make_update(chat_id=200, message_id=2), ctx)
        assert main._last_seen_msg[100] == 1
        assert main._last_seen_msg[200] == 2

    def teardown_method(self):
        main._last_seen_msg.clear()


# ------------------------------------------------------------------ #
# handle_message — routing logic                                       #
# ------------------------------------------------------------------ #


class TestHandleMessage:
    @pytest.mark.asyncio
    async def test_private_chat_responds(self):
        update = _make_update(text="hi there")
        ctx = _make_ctx()
        main._allowed = set()
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_awaited_once_with(update, ctx, "hi there")

    @pytest.mark.asyncio
    async def test_blocked_by_allowlist(self):
        update = _make_update(chat_id=100)
        ctx = _make_ctx()
        main._allowed = {999}
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_group_ignores_without_mention(self):
        update = _make_update(
            chat_type=ChatType.GROUP, text="random message", chat_title="Test"
        )
        ctx = _make_ctx(bot_username="testbot")
        main._allowed = set()
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_group_responds_to_mention(self):
        update = _make_update(
            chat_type=ChatType.GROUP,
            text="@testbot what do you think?",
            chat_title="Test",
        )
        ctx = _make_ctx(bot_username="testbot")
        main._allowed = set()
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_awaited_once_with(update, ctx, "what do you think?")

    @pytest.mark.asyncio
    async def test_group_mention_case_insensitive(self):
        update = _make_update(
            chat_type=ChatType.GROUP,
            text="@TestBot please help",
            chat_title="Test",
        )
        ctx = _make_ctx(bot_username="testbot")
        main._allowed = set()
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_group_responds_to_reply_to_bot(self):
        bot_msg = MagicMock()
        bot_msg.from_user = MagicMock()
        bot_msg.from_user.id = 999

        update = _make_update(
            chat_type=ChatType.SUPERGROUP,
            text="thanks!",
            chat_title="Dev",
            reply_to_message=bot_msg,
        )
        ctx = _make_ctx(bot_id=999, bot_username="testbot")
        main._allowed = set()
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_awaited_once_with(update, ctx, "thanks!")

    @pytest.mark.asyncio
    async def test_group_mention_only_is_ignored(self):
        update = _make_update(
            chat_type=ChatType.GROUP, text="@testbot", chat_title="Test"
        )
        ctx = _make_ctx(bot_username="testbot")
        main._allowed = set()
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_group_reply_to_non_bot_user_ignored(self):
        other_msg = MagicMock()
        other_msg.from_user = MagicMock()
        other_msg.from_user.id = 555  # not the bot

        update = _make_update(
            chat_type=ChatType.GROUP,
            text="I agree!",
            chat_title="Test",
            reply_to_message=other_msg,
        )
        ctx = _make_ctx(bot_id=999, bot_username="testbot")
        main._allowed = set()
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_empty_text_in_private_chat(self):
        update = _make_update(text="")
        ctx = _make_ctx()
        main._allowed = set()
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_awaited_once_with(update, ctx, "")

    @pytest.mark.asyncio
    async def test_none_text_treated_as_empty(self):
        update = _make_update(text=None)
        ctx = _make_ctx()
        main._allowed = set()
        with patch.object(main, "_respond", new_callable=AsyncMock) as mock_respond:
            await main.handle_message(update, ctx)
            mock_respond.assert_awaited_once_with(update, ctx, "")

    def teardown_method(self):
        main._allowed = set()


# ------------------------------------------------------------------ #
# _respond — response handling                                         #
# ------------------------------------------------------------------ #


class TestRespond:
    @pytest.fixture(autouse=True)
    def _setup(self):
        self.mock_acp = MagicMock()
        main._acp = self.mock_acp
        main._allowed = set()
        main._last_seen_msg.clear()
        yield
        main._acp = None
        main._allowed = set()
        main._last_seen_msg.clear()

    @pytest.mark.asyncio
    async def test_sends_response_as_new_message_when_latest(self):
        update = _make_update(chat_id=100, message_id=5)
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        async def fake_prompt(*_args, **_kwargs):
            yield "Hello back!"

        self.mock_acp.prompt = MagicMock(side_effect=fake_prompt)

        await main._respond(update, ctx, "hi")

        ctx.bot.send_chat_action.assert_awaited_once()
        ctx.bot.send_message.assert_awaited_once_with(100, "Hello back!")
        update.message.reply_text.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_replies_with_threading_when_not_latest(self):
        update = _make_update(chat_id=100, message_id=5)
        ctx = _make_ctx()
        main._last_seen_msg[100] = 10

        async def fake_prompt(*_args, **_kwargs):
            yield "threaded reply"

        self.mock_acp.prompt = MagicMock(side_effect=fake_prompt)

        await main._respond(update, ctx, "hi")

        update.message.reply_text.assert_awaited_once_with("threaded reply")
        ctx.bot.send_message.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_empty_response_sends_nothing(self):
        update = _make_update(chat_id=100, message_id=5)
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        async def fake_prompt(*_args, **_kwargs):
            yield "   "

        self.mock_acp.prompt = MagicMock(side_effect=fake_prompt)

        await main._respond(update, ctx, "hi")

        ctx.bot.send_message.assert_not_awaited()
        update.message.reply_text.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_multi_chunk_response(self):
        update = _make_update(chat_id=100, message_id=5)
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        async def fake_prompt(*_args, **_kwargs):
            yield "chunk1 "
            yield "chunk2 "
            yield "chunk3"

        self.mock_acp.prompt = MagicMock(side_effect=fake_prompt)

        await main._respond(update, ctx, "hi")

        ctx.bot.send_message.assert_awaited_once_with(100, "chunk1 chunk2 chunk3")

    @pytest.mark.asyncio
    async def test_long_response_split_into_parts(self):
        update = _make_update(chat_id=100, message_id=5)
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        # Generate a response longer than 4096 chars
        long_text = "x" * 5000

        async def fake_prompt(*_args, **_kwargs):
            yield long_text

        self.mock_acp.prompt = MagicMock(side_effect=fake_prompt)

        await main._respond(update, ctx, "hi")

        assert ctx.bot.send_message.await_count == 2
        first_call = ctx.bot.send_message.await_args_list[0]
        second_call = ctx.bot.send_message.await_args_list[1]
        assert len(first_call.args[1]) == 4096
        assert len(second_call.args[1]) == 904

    @pytest.mark.asyncio
    async def test_error_resets_session(self):
        update = _make_update(chat_id=100, message_id=5)
        ctx = _make_ctx()
        self.mock_acp.reset = AsyncMock()

        async def failing_prompt(*_args, **_kwargs):
            raise RuntimeError("boom")
            yield  # pragma: no cover

        self.mock_acp.prompt = MagicMock(side_effect=failing_prompt)

        await main._respond(update, ctx, "hi")

        update.message.reply_text.assert_awaited_with(
            "Something went wrong. Try /reset and send again."
        )
        self.mock_acp.reset.assert_awaited_once_with(100)

    @pytest.mark.asyncio
    async def test_builds_enriched_prompt_for_group(self):
        update = _make_update(
            chat_id=100,
            chat_type=ChatType.GROUP,
            chat_title="Book Club",
            message_id=5,
            user_full_name="Alice",
        )
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        captured_blocks = []

        async def capture_prompt(_chat_id, blocks, **_kwargs):
            captured_blocks.extend(blocks)
            yield "ok"

        self.mock_acp.prompt = MagicMock(side_effect=capture_prompt)

        await main._respond(update, ctx, "hello world")

        assert len(captured_blocks) == 1
        block_text = captured_blocks[0].text
        assert '[Chat: "Book Club" | group]' in block_text
        assert "[Alice]: hello world" in block_text

    @pytest.mark.asyncio
    async def test_builds_enriched_prompt_with_reply_context(self):
        reply_msg = MagicMock()
        reply_msg.text = "original question"
        reply_msg.from_user = MagicMock()
        reply_msg.from_user.id = 300
        reply_msg.from_user.full_name = "Bob"
        reply_msg.from_user.username = "bob"

        update = _make_update(
            chat_id=100,
            message_id=5,
            reply_to_message=reply_msg,
        )
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        captured_blocks = []

        async def capture_prompt(_chat_id, blocks, **_kwargs):
            captured_blocks.extend(blocks)
            yield "ok"

        self.mock_acp.prompt = MagicMock(side_effect=capture_prompt)

        await main._respond(update, ctx, "my answer")

        block_text = captured_blocks[0].text
        assert '[Replying to Bob: "original question"]' in block_text
        assert "[Alice Smith]: my answer" in block_text

    @pytest.mark.asyncio
    async def test_private_chat_no_header(self):
        update = _make_update(
            chat_id=100,
            chat_type=ChatType.PRIVATE,
            message_id=5,
        )
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        captured_blocks = []

        async def capture_prompt(_chat_id, blocks, **_kwargs):
            captured_blocks.extend(blocks)
            yield "ok"

        self.mock_acp.prompt = MagicMock(side_effect=capture_prompt)

        await main._respond(update, ctx, "hello")

        block_text = captured_blocks[0].text
        assert "[Chat:" not in block_text
        assert "[Alice Smith]: hello" in block_text

    @pytest.mark.asyncio
    async def test_display_name_fallback_to_username(self):
        update = _make_update(
            user_full_name=None,
            user_username="alice42",
            message_id=5,
        )
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        captured_blocks = []

        async def capture_prompt(_chat_id, blocks, **_kwargs):
            captured_blocks.extend(blocks)
            yield "ok"

        self.mock_acp.prompt = MagicMock(side_effect=capture_prompt)

        await main._respond(update, ctx, "hello")

        block_text = captured_blocks[0].text
        assert "[alice42]: hello" in block_text

    @pytest.mark.asyncio
    async def test_display_name_fallback_to_id(self):
        update = _make_update(
            user_full_name=None,
            user_username=None,
            user_id=12345,
            message_id=5,
        )
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        captured_blocks = []

        async def capture_prompt(_chat_id, blocks, **_kwargs):
            captured_blocks.extend(blocks)
            yield "ok"

        self.mock_acp.prompt = MagicMock(side_effect=capture_prompt)

        await main._respond(update, ctx, "hello")

        block_text = captured_blocks[0].text
        assert "[12345]: hello" in block_text

    @pytest.mark.asyncio
    async def test_passes_meta_with_principal_and_context(self):
        update = _make_update(
            chat_id=100,
            chat_type=ChatType.SUPERGROUP,
            chat_title="Dev Team",
            message_id=5,
            user_id=200,
            user_full_name="Alice Smith",
            user_username="alice",
        )
        ctx = _make_ctx()
        main._last_seen_msg[100] = 5

        captured_meta = {}

        async def capture_prompt(_chat_id, _blocks, **kwargs):
            captured_meta.update(kwargs.get("meta", {}))
            yield "ok"

        self.mock_acp.prompt = MagicMock(side_effect=capture_prompt)

        await main._respond(update, ctx, "hello")

        assert captured_meta["principal"]["platform"] == "telegram"
        assert captured_meta["principal"]["user_id"] == 200
        assert captured_meta["principal"]["display_name"] == "Alice Smith"
        assert captured_meta["principal"]["username"] == "alice"
        assert captured_meta["context"]["chat_id"] == 100
        assert captured_meta["context"]["chat_type"] == "supergroup"
        assert captured_meta["context"]["chat_title"] == "Dev Team"


# ------------------------------------------------------------------ #
# Lifecycle                                                            #
# ------------------------------------------------------------------ #


class TestLifecycle:
    @pytest.mark.asyncio
    async def test_post_init_starts_acp(self):
        mock_acp = AsyncMock()
        main._acp = mock_acp
        await main.post_init(MagicMock())
        mock_acp.start.assert_awaited_once()
        main._acp = None

    @pytest.mark.asyncio
    async def test_post_shutdown_stops_acp(self):
        mock_acp = AsyncMock()
        main._acp = mock_acp
        await main.post_shutdown(MagicMock())
        mock_acp.stop.assert_awaited_once()
        main._acp = None

    @pytest.mark.asyncio
    async def test_post_init_logs_allowlist(self):
        mock_acp = AsyncMock()
        main._acp = mock_acp
        main._allowed = {100, 200}
        with patch.object(main.logger, "info") as mock_log:
            await main.post_init(MagicMock())
            mock_log.assert_called_once()
            log_msg = mock_log.call_args[0][1]
            assert 100 in log_msg
            assert 200 in log_msg
        main._acp = None
        main._allowed = set()


# ------------------------------------------------------------------ #
# CLI argument parsing                                                 #
# ------------------------------------------------------------------ #


class TestParseArgs:
    def test_no_args(self):
        args = main._parse_args([])
        assert args.token is None
        assert args.acp_cmd is None
        assert args.session_mode is None
        assert args.allowed_chats is None
        assert args.debug is None

    def test_all_flags(self):
        args = main._parse_args([
            "--token", "abc:123",
            "--acp-cmd", "my-agent serve",
            "--session-mode", "chat",
            "--allowed-chats", "100,-200",
            "--debug",
        ])
        assert args.token == "abc:123"
        assert args.acp_cmd == "my-agent serve"
        assert args.session_mode == "chat"
        assert args.allowed_chats == "100,-200"
        assert args.debug is True

    def test_partial_flags(self):
        args = main._parse_args(["--token", "xyz"])
        assert args.token == "xyz"
        assert args.acp_cmd is None
        assert args.debug is None

    def test_debug_flag_is_store_true(self):
        args = main._parse_args(["--debug"])
        assert args.debug is True

    def test_version(self):
        with pytest.raises(SystemExit) as exc_info:
            main._parse_args(["--version"])
        assert exc_info.value.code == 0
