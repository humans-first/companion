import asyncio
from contextlib import asynccontextmanager
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from telegram_acp.acp import ACPError, ACPManager, _BotClient

# ------------------------------------------------------------------ #
# _BotClient                                                           #
# ------------------------------------------------------------------ #


class TestBotClient:
    def test_set_and_clear_queue(self):
        client = _BotClient()
        q = asyncio.Queue()
        client.set_queue("sess-1", q)
        assert client._queues["sess-1"] is q
        client.clear_queue("sess-1")
        assert "sess-1" not in client._queues

    def test_clear_missing_queue_is_noop(self):
        client = _BotClient()
        client.clear_queue("nonexistent")

    @pytest.mark.asyncio
    async def test_session_update_routes_to_queue(self):
        client = _BotClient()
        q = asyncio.Queue()
        client.set_queue("sess-1", q)
        await client.session_update("sess-1", "chunk-data")
        assert q.get_nowait() == "chunk-data"

    @pytest.mark.asyncio
    async def test_session_update_ignores_unknown_session(self):
        client = _BotClient()
        await client.session_update("unknown", "data")

    @pytest.mark.asyncio
    async def test_session_update_isolates_sessions(self):
        client = _BotClient()
        q1 = asyncio.Queue()
        q2 = asyncio.Queue()
        client.set_queue("sess-1", q1)
        client.set_queue("sess-2", q2)
        await client.session_update("sess-1", "for-1")
        await client.session_update("sess-2", "for-2")
        assert q1.get_nowait() == "for-1"
        assert q2.get_nowait() == "for-2"
        assert q1.empty()
        assert q2.empty()

    @pytest.mark.asyncio
    async def test_many_updates_same_session(self):
        client = _BotClient()
        q = asyncio.Queue()
        client.set_queue("sess-1", q)
        for i in range(50):
            await client.session_update("sess-1", f"chunk-{i}")
        assert q.qsize() == 50
        for i in range(50):
            assert q.get_nowait() == f"chunk-{i}"

    def test_overwrite_queue_for_session(self):
        client = _BotClient()
        q1 = asyncio.Queue()
        q2 = asyncio.Queue()
        client.set_queue("sess-1", q1)
        client.set_queue("sess-1", q2)
        assert client._queues["sess-1"] is q2


class TestBotClientStubs:
    """Verify stub methods return expected defaults."""

    @pytest.mark.asyncio
    async def test_request_permission(self):
        result = await _BotClient().request_permission(None, None, None)
        assert result == {"outcome": {"outcome": "approved"}}

    @pytest.mark.asyncio
    async def test_write_text_file(self):
        assert await _BotClient().write_text_file("c", "p", "s") is None

    @pytest.mark.asyncio
    async def test_read_text_file(self):
        assert await _BotClient().read_text_file("p", "s") == {"content": ""}

    @pytest.mark.asyncio
    async def test_create_terminal(self):
        assert await _BotClient().create_terminal("cmd", "s") == {"terminalId": "noop"}

    @pytest.mark.asyncio
    async def test_terminal_output(self):
        assert await _BotClient().terminal_output("s", "t") == {"output": ""}

    @pytest.mark.asyncio
    async def test_release_terminal(self):
        assert await _BotClient().release_terminal("s", "t") is None

    @pytest.mark.asyncio
    async def test_wait_for_terminal_exit(self):
        assert await _BotClient().wait_for_terminal_exit("s", "t") == {"exitCode": 0}

    @pytest.mark.asyncio
    async def test_kill_terminal(self):
        assert await _BotClient().kill_terminal("s", "t") is None

    @pytest.mark.asyncio
    async def test_ext_method(self):
        assert await _BotClient().ext_method("m", {}) == {}

    @pytest.mark.asyncio
    async def test_ext_notification(self):
        assert await _BotClient().ext_notification("m", {}) is None

    def test_on_connect(self):
        _BotClient().on_connect(None)


# ------------------------------------------------------------------ #
# ACPManager helpers                                                   #
# ------------------------------------------------------------------ #


def _mock_spawn(conn=None, session_id="sess-abc"):
    """Create a mock spawn_agent_process context manager."""
    if conn is None:
        conn = AsyncMock()
        conn.initialize = AsyncMock()
        session = SimpleNamespace(session_id=session_id)
        conn.new_session = AsyncMock(return_value=session)
        conn.set_session_mode = AsyncMock()
        conn.prompt = AsyncMock()
    proc = MagicMock()
    proc.pid = 12345
    proc.returncode = 0

    @asynccontextmanager
    async def fake_spawn(*_args, **_kwargs):
        yield conn, proc

    return fake_spawn, conn, proc


def _mock_spawn_sequential(session_ids):
    """Create a spawn that returns different session IDs for sequential new_session calls."""
    conn = AsyncMock()
    conn.initialize = AsyncMock()
    call_count = 0

    async def new_session_side_effect(**_kwargs):
        nonlocal call_count
        sid = session_ids[call_count % len(session_ids)]
        call_count += 1
        return SimpleNamespace(session_id=sid)

    conn.new_session = AsyncMock(side_effect=new_session_side_effect)
    conn.set_session_mode = AsyncMock()
    conn.prompt = AsyncMock()

    proc = MagicMock()
    proc.pid = 12345
    proc.returncode = 0

    @asynccontextmanager
    async def fake_spawn(*_args, **_kwargs):
        yield conn, proc

    return fake_spawn, conn, proc


# ------------------------------------------------------------------ #
# ACPManager — basic state                                             #
# ------------------------------------------------------------------ #


class TestACPManagerSessionMapping:
    def test_initial_state(self):
        mgr = ACPManager(cmd=["echo"])
        assert mgr._sessions == {}
        assert mgr._locks == {}

    def test_get_lock_creates_once(self):
        mgr = ACPManager(cmd=["echo"])
        lock1 = mgr._get_lock(100)
        lock2 = mgr._get_lock(100)
        assert lock1 is lock2
        assert isinstance(lock1, asyncio.Lock)

    def test_get_lock_per_chat(self):
        mgr = ACPManager(cmd=["echo"])
        lock_a = mgr._get_lock(100)
        lock_b = mgr._get_lock(200)
        assert lock_a is not lock_b

    @pytest.mark.asyncio
    async def test_reset_removes_session(self):
        mgr = ACPManager(cmd=["echo"])
        mgr._sessions[100] = "sess-abc"
        await mgr.reset(100)
        assert 100 not in mgr._sessions

    @pytest.mark.asyncio
    async def test_reset_missing_chat_is_noop(self):
        mgr = ACPManager(cmd=["echo"])
        await mgr.reset(999)

    def test_is_alive_false_initially(self):
        mgr = ACPManager(cmd=["echo"])
        assert mgr._is_alive() is False

    @pytest.mark.asyncio
    async def test_stop_without_start_is_safe(self):
        mgr = ACPManager(cmd=["echo"])
        await mgr.stop()


# ------------------------------------------------------------------ #
# ACPManager — lifecycle happy paths                                   #
# ------------------------------------------------------------------ #


class TestACPManagerLifecycle:
    @pytest.mark.asyncio
    async def test_start_and_stop(self):
        fake_spawn, conn, _ = _mock_spawn()
        mgr = ACPManager(cmd=["test", "cmd"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            assert mgr._conn is conn
            assert mgr._is_alive()
            conn.initialize.assert_awaited_once_with(protocol_version=1)

            await mgr.stop()

    @pytest.mark.asyncio
    async def test_start_with_session_mode(self):
        fake_spawn, conn, _ = _mock_spawn()
        mgr = ACPManager(cmd=["test"], session_mode="chat")

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            await mgr._get_session(100)
            conn.set_session_mode.assert_awaited_once_with(mode_id="chat", session_id="sess-abc")
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_start_without_session_mode_skips_set(self):
        fake_spawn, conn, _ = _mock_spawn()
        mgr = ACPManager(cmd=["test"], session_mode="")

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            await mgr._get_session(100)
            conn.set_session_mode.assert_not_awaited()
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_get_session_creates_and_caches(self):
        fake_spawn, conn, _ = _mock_spawn()
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            sid = await mgr._get_session(100)
            assert sid == "sess-abc"
            assert mgr._sessions[100] == "sess-abc"

            # Second call returns cached session
            sid2 = await mgr._get_session(100)
            assert sid2 == "sess-abc"
            conn.new_session.assert_awaited_once()

            await mgr.stop()

    @pytest.mark.asyncio
    async def test_different_chats_get_different_sessions(self):
        fake_spawn, conn, _ = _mock_spawn_sequential(["sess-1", "sess-2", "sess-3"])
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            s1 = await mgr._get_session(100)
            s2 = await mgr._get_session(200)
            s3 = await mgr._get_session(300)

            assert s1 == "sess-1"
            assert s2 == "sess-2"
            assert s3 == "sess-3"
            assert conn.new_session.await_count == 3

            await mgr.stop()

    @pytest.mark.asyncio
    async def test_ensure_process_respawns(self):
        fake_spawn, _conn, _ = _mock_spawn()
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            mgr._sessions[100] = "old-session"

            # Simulate process death
            await mgr.stop()
            assert not mgr._is_alive()

            # _ensure_process should respawn
            await mgr._ensure_process()
            assert mgr._is_alive()
            assert mgr._sessions == {}  # sessions cleared on respawn

            await mgr.stop()

    @pytest.mark.asyncio
    async def test_ensure_process_noop_when_alive(self):
        fake_spawn, _, _ = _mock_spawn()
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            ready_event = mgr._ready  # save reference

            await mgr._ensure_process()
            # Should not have replaced the event (no respawn happened)
            assert mgr._ready is ready_event

            await mgr.stop()

    @pytest.mark.asyncio
    async def test_reset_then_new_session(self):
        fake_spawn, _conn, _ = _mock_spawn_sequential(["sess-old", "sess-new"])
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            s1 = await mgr._get_session(100)
            assert s1 == "sess-old"

            await mgr.reset(100)
            assert 100 not in mgr._sessions

            s2 = await mgr._get_session(100)
            assert s2 == "sess-new"

            await mgr.stop()


# ------------------------------------------------------------------ #
# ACPManager — failure modes                                           #
# ------------------------------------------------------------------ #


class TestACPManagerFailures:
    @pytest.mark.asyncio
    async def test_start_failure_raises_acp_error(self):
        @asynccontextmanager
        async def failing_spawn(*_args, **_kwargs):
            raise RuntimeError("spawn failed")
            yield  # pragma: no cover

        mgr = ACPManager(cmd=["test"])
        with (
            patch("telegram_acp.acp.spawn_agent_process", failing_spawn),
            pytest.raises(ACPError, match="ACP process failed to start"),
        ):
            await mgr.start()

    @pytest.mark.asyncio
    async def test_conn_is_none_after_failure(self):
        @asynccontextmanager
        async def failing_spawn(*_args, **_kwargs):
            raise RuntimeError("spawn failed")
            yield  # pragma: no cover

        mgr = ACPManager(cmd=["test"])
        with (
            patch("telegram_acp.acp.spawn_agent_process", failing_spawn),
            pytest.raises(ACPError),
        ):
            await mgr.start()

        assert mgr._conn is None

    @pytest.mark.asyncio
    async def test_stop_force_kills_stuck_process(self):
        fake_spawn, _, proc = _mock_spawn()
        proc.returncode = None  # process still alive
        proc.kill = MagicMock()
        proc.wait = AsyncMock()

        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            mgr._proc = proc
            await mgr.stop()
            proc.kill.assert_called_once()
            proc.wait.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_stop_does_not_kill_already_exited_process(self):
        fake_spawn, _, proc = _mock_spawn()
        proc.returncode = 0  # already exited
        proc.kill = MagicMock()

        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            mgr._proc = proc
            await mgr.stop()
            proc.kill.assert_not_called()

    @pytest.mark.asyncio
    async def test_double_stop_is_safe(self):
        fake_spawn, _, _ = _mock_spawn()
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            await mgr.stop()
            await mgr.stop()  # second stop should not raise

    @pytest.mark.asyncio
    async def test_reset_all_chats(self):
        mgr = ACPManager(cmd=["echo"])
        mgr._sessions = {1: "s1", 2: "s2", 3: "s3"}
        await mgr.reset(1)
        await mgr.reset(2)
        await mgr.reset(3)
        assert mgr._sessions == {}


# ------------------------------------------------------------------ #
# ACPManager — prompt streaming                                        #
# ------------------------------------------------------------------ #


class TestACPManagerPrompt:
    @pytest.mark.asyncio
    async def test_prompt_streams_chunks(self):
        from acp.schema import AgentMessageChunk

        fake_spawn, conn, _ = _mock_spawn()

        async def fake_prompt(session_id, prompt):
            q = mgr._client._queues.get(session_id)
            if q:
                chunk1 = MagicMock(spec=AgentMessageChunk)
                chunk1.content = SimpleNamespace(text="Hello ")
                chunk2 = MagicMock(spec=AgentMessageChunk)
                chunk2.content = SimpleNamespace(text="world")
                await q.put(chunk1)
                await q.put(chunk2)

        conn.prompt = AsyncMock(side_effect=fake_prompt)

        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            chunks = []
            async for text in mgr.prompt(100, []):
                chunks.append(text)

            assert chunks == ["Hello ", "world"]
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_prompt_skips_non_text_chunks(self):
        from acp.schema import AgentMessageChunk

        fake_spawn, conn, _ = _mock_spawn()

        async def fake_prompt(session_id, prompt):
            q = mgr._client._queues.get(session_id)
            if q:
                chunk = MagicMock(spec=AgentMessageChunk)
                chunk.content = SimpleNamespace(text=None)  # no text
                await q.put(chunk)
                await q.put("not-a-chunk")  # non-AgentMessageChunk

        conn.prompt = AsyncMock(side_effect=fake_prompt)
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            chunks = []
            async for text in mgr.prompt(100, []):
                chunks.append(text)

            assert chunks == []
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_prompt_creates_session_lazily(self):
        fake_spawn, conn, _ = _mock_spawn()
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            assert mgr._sessions == {}

            conn.prompt = AsyncMock()  # prompt returns immediately

            async for _ in mgr.prompt(100, []):
                pass

            assert 100 in mgr._sessions
            conn.new_session.assert_awaited_once()

            await mgr.stop()

    @pytest.mark.asyncio
    async def test_prompt_reuses_existing_session(self):
        fake_spawn, conn, _ = _mock_spawn()
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()
            conn.prompt = AsyncMock()

            async for _ in mgr.prompt(100, []):
                pass
            async for _ in mgr.prompt(100, []):
                pass

            conn.new_session.assert_awaited_once()  # only created once
            assert conn.prompt.await_count == 2
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_prompt_many_chunks(self):
        from acp.schema import AgentMessageChunk

        fake_spawn, conn, _ = _mock_spawn()

        async def fake_prompt(session_id, prompt):
            q = mgr._client._queues.get(session_id)
            if q:
                for i in range(100):
                    chunk = MagicMock(spec=AgentMessageChunk)
                    chunk.content = SimpleNamespace(text=f"c{i}")
                    await q.put(chunk)

        conn.prompt = AsyncMock(side_effect=fake_prompt)
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            chunks = []
            async for text in mgr.prompt(100, []):
                chunks.append(text)

            assert len(chunks) == 100
            assert chunks[0] == "c0"
            assert chunks[99] == "c99"
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_prompt_empty_response(self):
        fake_spawn, conn, _ = _mock_spawn()
        conn.prompt = AsyncMock()  # returns immediately, no chunks
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            chunks = []
            async for text in mgr.prompt(100, []):
                chunks.append(text)

            assert chunks == []
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_prompt_queue_cleaned_up_after_completion(self):
        fake_spawn, conn, _ = _mock_spawn()
        conn.prompt = AsyncMock()
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            async for _ in mgr.prompt(100, []):
                pass

            # Queue should be cleaned up
            sid = mgr._sessions[100]
            assert sid not in mgr._client._queues
            await mgr.stop()


# ------------------------------------------------------------------ #
# ACPManager — concurrency                                            #
# ------------------------------------------------------------------ #


class TestACPManagerConcurrency:
    @pytest.mark.asyncio
    async def test_concurrent_prompts_different_chats(self):
        """Different chats should be able to prompt concurrently."""
        from acp.schema import AgentMessageChunk

        fake_spawn, conn, _ = _mock_spawn_sequential(["sess-1", "sess-2", "sess-3"])

        # Track which sessions are concurrently active
        active = set()
        max_concurrent = 0

        async def fake_prompt(session_id, prompt):
            nonlocal max_concurrent
            active.add(session_id)
            max_concurrent = max(max_concurrent, len(active))
            q = mgr._client._queues.get(session_id)
            if q:
                chunk = MagicMock(spec=AgentMessageChunk)
                chunk.content = SimpleNamespace(text=f"reply-{session_id}")
                await q.put(chunk)
            await asyncio.sleep(0.01)  # simulate work
            active.discard(session_id)

        conn.prompt = AsyncMock(side_effect=fake_prompt)
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            async def prompt_chat(chat_id):
                chunks = []
                async for text in mgr.prompt(chat_id, []):
                    chunks.append(text)
                return chunks

            results = await asyncio.gather(
                prompt_chat(100),
                prompt_chat(200),
                prompt_chat(300),
            )

            assert len(results) == 3
            # Each chat got its own session's response
            for r in results:
                assert len(r) == 1
                assert r[0].startswith("reply-sess-")

            # At least 2 were concurrent at some point (with the sleep)
            assert max_concurrent >= 2

            await mgr.stop()

    @pytest.mark.asyncio
    async def test_sequential_prompts_same_chat(self):
        """Prompts to the same chat should be serialized by the per-chat lock."""
        from acp.schema import AgentMessageChunk

        fake_spawn, conn, _ = _mock_spawn()
        order = []

        async def fake_prompt(session_id, prompt):
            q = mgr._client._queues.get(session_id)
            call_num = len(order)
            order.append(f"start-{call_num}")
            if q:
                chunk = MagicMock(spec=AgentMessageChunk)
                chunk.content = SimpleNamespace(text=f"reply-{call_num}")
                await q.put(chunk)
            await asyncio.sleep(0.01)
            order.append(f"end-{call_num}")

        conn.prompt = AsyncMock(side_effect=fake_prompt)
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            async def prompt_same_chat(n):
                chunks = []
                async for text in mgr.prompt(100, []):
                    chunks.append(text)
                return chunks

            # Fire off 3 prompts to the same chat concurrently
            results = await asyncio.gather(
                prompt_same_chat(0),
                prompt_same_chat(1),
                prompt_same_chat(2),
            )

            # All 3 should have completed
            assert len(results) == 3

            # They should have been serialized: start-0, end-0, start-1, end-1, ...
            # Verify no interleaving
            for i in range(0, len(order) - 1, 2):
                start_idx = int(order[i].split("-")[1])
                end_idx = int(order[i + 1].split("-")[1])
                assert start_idx == end_idx, f"Interleaved: {order}"

            await mgr.stop()

    @pytest.mark.asyncio
    async def test_per_chat_locks_are_independent(self):
        """Locks for different chats should not block each other."""
        mgr = ACPManager(cmd=["echo"])

        lock_a = mgr._get_lock(100)
        lock_b = mgr._get_lock(200)

        # Acquire lock_a, lock_b should still be acquirable
        await lock_a.acquire()
        assert not lock_b.locked()
        await lock_b.acquire()

        lock_a.release()
        lock_b.release()

    @pytest.mark.asyncio
    async def test_concurrent_reset_and_prompt(self):
        """Reset during a prompt to a different chat should not interfere."""
        fake_spawn, conn, _ = _mock_spawn_sequential(["sess-1", "sess-2"])
        conn.prompt = AsyncMock()
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            # Create session for chat 100
            async for _ in mgr.prompt(100, []):
                pass
            assert 100 in mgr._sessions

            # Reset chat 100 while prompting chat 200
            async def reset_100():
                await mgr.reset(100)

            async def prompt_200():
                async for _ in mgr.prompt(200, []):
                    pass

            await asyncio.gather(reset_100(), prompt_200())

            assert 100 not in mgr._sessions
            assert 200 in mgr._sessions

            await mgr.stop()

    @pytest.mark.asyncio
    async def test_queue_isolation_under_concurrency(self):
        """Concurrent prompts to different chats must not mix their chunks."""
        from acp.schema import AgentMessageChunk

        fake_spawn, conn, _ = _mock_spawn_sequential(["sess-A", "sess-B"])

        async def fake_prompt(session_id, prompt):
            q = mgr._client._queues.get(session_id)
            if q:
                # Each session sends 5 chunks tagged with its session ID
                for i in range(5):
                    chunk = MagicMock(spec=AgentMessageChunk)
                    chunk.content = SimpleNamespace(text=f"{session_id}:{i}")
                    await q.put(chunk)
                    await asyncio.sleep(0)  # yield control

        conn.prompt = AsyncMock(side_effect=fake_prompt)
        mgr = ACPManager(cmd=["test"])

        with patch("telegram_acp.acp.spawn_agent_process", fake_spawn):
            await mgr.start()

            async def prompt_chat(chat_id):
                chunks = []
                async for text in mgr.prompt(chat_id, []):
                    chunks.append(text)
                return chunks

            results = await asyncio.gather(prompt_chat(100), prompt_chat(200))
            chunks_a, chunks_b = results

            # Each chat should only see its own session's chunks
            assert all(c.startswith("sess-A:") for c in chunks_a)
            assert all(c.startswith("sess-B:") for c in chunks_b)
            assert len(chunks_a) == 5
            assert len(chunks_b) == 5

            await mgr.stop()
