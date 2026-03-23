import asyncio
import json

import httpx
import pytest

from telegram_acp.acp import _BotClient
from telegram_acp.http_transport import connect_http_agent


class _QueueByteStream(httpx.AsyncByteStream):
    def __init__(self, queue: asyncio.Queue[bytes | None]) -> None:
        self._queue = queue

    async def __aiter__(self):
        while True:
            item = await self._queue.get()
            if item is None:
                break
            yield item

    async def aclose(self) -> None:
        return None


@pytest.mark.asyncio
async def test_connect_http_agent_round_trips_initialize_and_new_session():
    stream_queue: asyncio.Queue[bytes | None] = asyncio.Queue()
    seen_requests: list[dict[str, object]] = []
    delete_calls: list[str] = []

    async def handler(request: httpx.Request) -> httpx.Response:
        if request.method == "POST" and request.url.path == "/v1/acp/connections":
            return httpx.Response(
                status_code=201,
                json={
                    "connection_id": "conn-1",
                    "messages_path": "/v1/acp/connections/conn-1/messages",
                    "stream_path": "/v1/acp/connections/conn-1/stream",
                },
            )

        if request.method == "GET" and request.url.path == "/v1/acp/connections/conn-1/stream":
            return httpx.Response(
                status_code=200,
                headers={"content-type": "application/x-ndjson"},
                stream=_QueueByteStream(stream_queue),
            )

        if request.method == "POST" and request.url.path == "/v1/acp/connections/conn-1/messages":
            raw_body = await request.aread()
            assert not raw_body.endswith(b"\n")
            payload = json.loads(raw_body)
            seen_requests.append(payload)

            if payload["method"] == "initialize":
                result = {"protocolVersion": 1}
            else:
                result = {"sessionId": "sess-http"}

            response = {
                "jsonrpc": "2.0",
                "id": payload["id"],
                "result": result,
            }
            await stream_queue.put((json.dumps(response) + "\n").encode("utf-8"))
            return httpx.Response(status_code=202)

        if request.method == "DELETE" and request.url.path == "/v1/acp/connections/conn-1":
            delete_calls.append(request.url.path)
            await stream_queue.put(None)
            return httpx.Response(status_code=204)

        raise AssertionError(f"unexpected request: {request.method} {request.url}")

    transport = httpx.MockTransport(handler)

    async with connect_http_agent(
        _BotClient(),
        "https://gateway.example.test",
        transport=transport,
    ) as conn:
        init = await conn.initialize(protocol_version=1)
        session = await conn.new_session(cwd="/tmp", mcp_servers=[])

    assert init.protocol_version == 1
    assert session.session_id == "sess-http"
    assert [request["method"] for request in seen_requests] == ["initialize", "session/new"]
    assert delete_calls == ["/v1/acp/connections/conn-1"]
