"""
ACPManager: single ACP process, multiple sessions (one per chat).

Spawns one ACP subprocess and creates sessions on demand as new chats arrive.
Each chat gets its own session and its own lock, so concurrent chats can
prompt in parallel while prompts within a single chat are serialized.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import time
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any

from acp import spawn_agent_process
from acp.client.connection import ClientSideConnection
from acp.schema import (
    AgentMessageChunk,
    AudioContentBlock,
    EmbeddedResourceContentBlock,
    ImageContentBlock,
    ResourceContentBlock,
    TextContentBlock,
)
from opentelemetry import metrics, trace

ContentBlock = (
    TextContentBlock
    | ImageContentBlock
    | AudioContentBlock
    | ResourceContentBlock
    | EmbeddedResourceContentBlock
)

logger = logging.getLogger(__name__)
tracer = trace.get_tracer("telegram_acp.acp")
meter = metrics.get_meter("telegram_acp.acp")

prompt_duration = meter.create_histogram(
    "acp.prompt.duration",
    unit="s",
    description="Time from prompt send to full response received",
)
prompt_successes = meter.create_counter(
    "acp.prompt.successes",
    description="Number of successful ACP prompts",
)
prompt_failures = meter.create_counter(
    "acp.prompt.failures",
    description="Number of failed ACP prompts",
)
process_respawns = meter.create_counter(
    "acp.process.respawns",
    description="Number of times the ACP process was respawned",
)
active_sessions = meter.create_up_down_counter(
    "acp.sessions.active",
    description="Number of active ACP sessions",
)


class ACPError(Exception):
    pass


class _BotClient:
    """
    Minimal ACP Client implementation.

    Routes session_update callbacks to per-session queues so each prompt
    receives only its own session's streaming chunks.
    """

    def __init__(self) -> None:
        self._queues: dict[str, asyncio.Queue[Any]] = {}

    def set_queue(self, session_id: str, q: asyncio.Queue[Any]) -> None:
        self._queues[session_id] = q

    def clear_queue(self, session_id: str) -> None:
        self._queues.pop(session_id, None)

    async def session_update(self, session_id: str, update: Any, **_kwargs: Any) -> None:
        q = self._queues.get(session_id)
        if q is not None:
            await q.put(update)

    async def request_permission(
        self, _options: Any, _session_id: Any, _tool_call: Any, **_kwargs: Any
    ) -> dict[str, Any]:
        return {"outcome": {"outcome": "approved"}}

    async def write_text_file(
        self, _content: Any, _path: Any, _session_id: Any, **_kwargs: Any
    ) -> None:
        return None

    async def read_text_file(
        self, _path: Any, _session_id: Any, **_kwargs: Any
    ) -> dict[str, Any]:
        return {"content": ""}

    async def create_terminal(
        self, _command: Any, _session_id: Any, **_kwargs: Any
    ) -> dict[str, Any]:
        return {"terminalId": "noop"}

    async def terminal_output(
        self, _session_id: Any, _terminal_id: Any, **_kwargs: Any
    ) -> dict[str, Any]:
        return {"output": ""}

    async def release_terminal(
        self, _session_id: Any, _terminal_id: Any, **_kwargs: Any
    ) -> None:
        return None

    async def wait_for_terminal_exit(
        self, _session_id: Any, _terminal_id: Any, **_kwargs: Any
    ) -> dict[str, Any]:
        return {"exitCode": 0}

    async def kill_terminal(
        self, _session_id: Any, _terminal_id: Any, **_kwargs: Any
    ) -> None:
        return None

    async def ext_method(self, _method: Any, _params: Any) -> dict[str, Any]:
        return {}

    async def ext_notification(self, _method: Any, _params: Any) -> None:
        pass

    def on_connect(self, _conn: Any) -> None:
        pass


class ACPManager:
    """Single ACP process managing multiple chat sessions."""

    def __init__(self, cmd: list[str], session_mode: str = "", *, debug: bool = False):
        self.cmd = cmd
        self.session_mode = session_mode
        self.debug = debug
        self._client = _BotClient()
        self._conn: ClientSideConnection | None = None
        self._proc: asyncio.subprocess.Process | None = None
        self._ready = asyncio.Event()
        self._stop = asyncio.Event()
        self._task: asyncio.Task[None] | None = None
        self._sessions: dict[int, str] = {}  # chat_id -> session_id
        self._locks: dict[int, asyncio.Lock] = {}  # chat_id -> lock

    async def start(self) -> None:
        """Spawn the ACP subprocess and wait until it's ready."""
        with tracer.start_as_current_span(
            "acp.start",
            attributes={"acp.cmd": " ".join(self.cmd)},
        ) as span:
            self._task = asyncio.create_task(self._run(), name="acp-manager")
            await self._ready.wait()
            if self._conn is None:
                span.set_status(trace.StatusCode.ERROR, "ACP process failed to start")
                raise ACPError("ACP process failed to start")
            span.set_attribute("acp.pid", self._proc.pid if self._proc else -1)

    async def _run(self) -> None:
        try:
            async with spawn_agent_process(
                self._client,  # type: ignore[arg-type]  # duck-typed ACP Client
                self.cmd[0],
                *self.cmd[1:],
            ) as (conn, proc):
                self._conn = conn
                self._proc = proc
                await conn.initialize(protocol_version=1)
                logger.info("ACP process ready (pid=%s)", proc.pid)
                self._ready.set()
                await self._stop.wait()
        except Exception:
            logger.exception("ACP subprocess error")
        finally:
            self._conn = None
            if not self._ready.is_set():
                self._ready.set()

    def _is_alive(self) -> bool:
        return self._task is not None and not self._task.done()

    async def _ensure_process(self) -> None:
        """Respawn the ACP process if it died."""
        if not self._is_alive():
            logger.warning("ACP process not alive, respawning")
            process_respawns.add(1)
            self._ready = asyncio.Event()
            self._stop = asyncio.Event()
            self._sessions.clear()
            await self.start()

    async def _get_session(self, chat_id: int) -> str:
        """Get or create a session for the given chat."""
        if chat_id in self._sessions:
            return self._sessions[chat_id]

        with tracer.start_as_current_span(
            "acp.new_session",
            attributes={"chat.id": chat_id},
        ) as span:
            assert self._conn is not None
            session = await self._conn.new_session(cwd=str(Path.cwd()), mcp_servers=[])
            sid: str = session.session_id
            if self.session_mode:
                await self._conn.set_session_mode(mode_id=self.session_mode, session_id=sid)
            self._sessions[chat_id] = sid
            active_sessions.add(1)
            span.set_attribute("acp.session_id", sid)
            logger.info("New session for chat %d: %s", chat_id, sid)
            return sid

    def _get_lock(self, chat_id: int) -> asyncio.Lock:
        if chat_id not in self._locks:
            self._locks[chat_id] = asyncio.Lock()
        return self._locks[chat_id]

    async def prompt(self, chat_id: int, blocks: list[ContentBlock]) -> AsyncIterator[str]:
        """Send a prompt and yield text chunks as they stream in.

        Creates a session for the chat on first use. Serializes prompts
        within the same chat but allows concurrent prompts across chats.
        """
        with tracer.start_as_current_span(
            "acp.prompt",
            attributes={"chat.id": chat_id},
        ) as span:
            await self._ensure_process()
            lock = self._get_lock(chat_id)
            t0 = time.monotonic()
            failed = False

            async with lock:
                sid = await self._get_session(chat_id)
                span.set_attribute("acp.session_id", sid)
                q: asyncio.Queue[Any] = asyncio.Queue()
                self._client.set_queue(sid, q)

                async def _do_prompt() -> None:
                    try:
                        assert self._conn is not None
                        await self._conn.prompt(session_id=sid, prompt=blocks)
                    finally:
                        await q.put(None)

                prompt_task = asyncio.create_task(_do_prompt())
                chunk_count = 0
                try:
                    while True:
                        item = await q.get()
                        if item is None:
                            break
                        if isinstance(item, AgentMessageChunk):
                            t = getattr(item.content, "text", None)
                            if t:
                                chunk_count += 1
                                yield t
                except Exception:
                    failed = True
                    raise
                finally:
                    self._client.clear_queue(sid)
                    await prompt_task
                    elapsed = time.monotonic() - t0
                    span.set_attribute("acp.chunk_count", chunk_count)
                    prompt_duration.record(elapsed, {"chat.id": chat_id})
                    if failed:
                        prompt_failures.add(1, {"chat.id": chat_id})
                    else:
                        prompt_successes.add(1, {"chat.id": chat_id})

    async def reset(self, chat_id: int) -> None:
        """Drop the session for a chat. Next prompt will create a new one."""
        if self._sessions.pop(chat_id, None) is not None:
            active_sessions.add(-1)

    async def stop(self) -> None:
        """Shut down the ACP process."""
        with tracer.start_as_current_span("acp.stop"):
            self._stop.set()
            if self._task:
                try:
                    await asyncio.wait_for(self._task, timeout=5)
                except TimeoutError:
                    self._task.cancel()
                    with contextlib.suppress(asyncio.CancelledError, Exception):
                        await self._task
            if self._proc and self._proc.returncode is None:
                logger.warning("Force-killing ACP subprocess pid=%s", self._proc.pid)
                self._proc.kill()
                await self._proc.wait()
