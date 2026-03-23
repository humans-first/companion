from __future__ import annotations

import asyncio
import contextlib
from collections.abc import AsyncIterator, Callable
from dataclasses import dataclass
from typing import Any

import httpx
from acp.client.connection import ClientSideConnection


@dataclass(frozen=True)
class _ConnectionDescriptor:
    connection_id: str
    messages_path: str
    stream_path: str


class _HttpStreamWriter(asyncio.StreamWriter):
    def __init__(self, client: httpx.AsyncClient, messages_path: str) -> None:
        self._client = client
        self._messages_path = messages_path
        self._buffer = bytearray()
        self._closed = False
        self._drain_lock = asyncio.Lock()
        self._transport = self

    def write(self, data: bytes | bytearray | memoryview) -> None:
        if self._closed:
            raise ConnectionError("HTTP ACP transport is closed")
        self._buffer.extend(bytes(data))

    async def drain(self) -> None:
        async with self._drain_lock:
            if self._closed:
                raise ConnectionError("HTTP ACP transport is closed")
            if not self._buffer:
                return

            payload = bytes(self._buffer)
            self._buffer.clear()
            if payload.endswith(b"\n"):
                payload = payload[:-1]

            response = await self._client.post(
                self._messages_path,
                content=payload,
                headers={"content-type": "application/json"},
            )
            if response.status_code == httpx.codes.ACCEPTED:
                return

            body = response.text.strip() or response.reason_phrase
            raise ConnectionError(
                f"HTTP ACP message post failed with {response.status_code}: {body}"
            )

    def close(self) -> None:
        self._closed = True
        self._buffer.clear()

    async def wait_closed(self) -> None:
        return None

    def is_closing(self) -> bool:
        return self._closed

    def can_write_eof(self) -> bool:
        return False

    def write_eof(self) -> None:
        raise NotImplementedError("HTTP ACP transport does not support write_eof()")

    def get_extra_info(self, _name: str, default: Any = None) -> Any:
        return default


async def _create_connection(client: httpx.AsyncClient) -> _ConnectionDescriptor:
    response = await client.post("/v1/acp/connections")
    response.raise_for_status()
    payload = response.json()

    try:
        return _ConnectionDescriptor(
            connection_id=str(payload["connection_id"]),
            messages_path=str(payload["messages_path"]),
            stream_path=str(payload["stream_path"]),
        )
    except KeyError as exc:  # pragma: no cover - defensive wire validation
        raise RuntimeError(f"HTTP ACP create response missing field: {exc.args[0]}") from exc


async def _delete_connection(
    client: httpx.AsyncClient,
    connection_id: str,
) -> None:
    with contextlib.suppress(httpx.HTTPError):
        await client.delete(f"/v1/acp/connections/{connection_id}")


async def _pump_stream(
    client: httpx.AsyncClient,
    stream_path: str,
    reader: asyncio.StreamReader,
) -> None:
    try:
        async with client.stream(
            "GET",
            stream_path,
            headers={"accept": "application/x-ndjson"},
        ) as response:
            response.raise_for_status()
            async for chunk in response.aiter_bytes():
                if chunk:
                    reader.feed_data(chunk)
    except asyncio.CancelledError:
        raise
    except Exception as exc:
        reader.set_exception(exc)
    finally:
        reader.feed_eof()


@contextlib.asynccontextmanager
async def connect_http_agent(
    to_client: Callable[[Any], Any] | Any,
    base_url: str,
    *,
    transport: httpx.AsyncBaseTransport | None = None,
    **connection_kwargs: Any,
) -> AsyncIterator[ClientSideConnection]:
    timeout = httpx.Timeout(connect=10.0, read=None, write=30.0, pool=10.0)
    client = httpx.AsyncClient(
        base_url=base_url,
        follow_redirects=True,
        timeout=timeout,
        transport=transport,
    )

    descriptor = await _create_connection(client)
    reader = asyncio.StreamReader()
    writer = _HttpStreamWriter(client, descriptor.messages_path)
    pump_task = asyncio.create_task(
        _pump_stream(client, descriptor.stream_path, reader),
        name="telegram-acp-http-stream",
    )

    try:
        conn = ClientSideConnection(to_client, writer, reader, **connection_kwargs)
        try:
            yield conn
        finally:
            await conn.close()
    finally:
        writer.close()
        pump_task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await pump_task
        await _delete_connection(client, descriptor.connection_id)
        await client.aclose()
