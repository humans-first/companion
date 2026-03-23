# acp-http-transport

`acp-http-transport` is a reusable Rust crate for speaking ACP over a custom streamable HTTP interface.

The crate is meant to sit underneath the normal ACP SDK, not beside it. It gives you a `TransportPeer` with an async reader, async writer, and shutdown future so you can wire it directly into:

- `agent_client_protocol::ClientSideConnection::new(...)`
- `agent_client_protocol::AgentSideConnection::new(...)`

That makes it useful in both directions:

- ACP clients that want to connect to a remote HTTP endpoint
- ACP servers that want to expose themselves over HTTP without reimplementing ACP

## Transport Shape

Each ACP transport connection is represented by four endpoints:

- `POST /v1/acp/connections`
- `GET /v1/acp/connections/{id}/stream`
- `POST /v1/acp/connections/{id}/messages`
- `DELETE /v1/acp/connections/{id}`

Messages are carried as newline-delimited JSON over the stream and message channel.

## Public API

- `connect(ClientConfig)` for the client side
- `serve(listener, HttpTransportConfig, factory)` for the server side
- `TransportPeer` for ACP SDK integration
- `ServerTlsConfig` for optional HTTPS serving

The server API is intentionally generic: the connection factory only has to return a `TransportPeer`. That is why the same crate can be reused by the gateway today and by other ACP servers later.

## Current Use In This Repo

The gateway uses this crate to expose ACP over HTTP while still speaking normal ACP internally to its backend agents.

## Limits

- The client side supports both `http://` and `https://` base URLs.
- The server side now uses a higher-level `axum` router and can serve HTTPS when you provide a certificate and key through `HttpTransportConfig`.
- one active stream consumer per connection
- bounded per-connection buffering
- custom ACP transport, not a standardized ACP transport built into the upstream SDK

## Development

```sh
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## License

Apache-2.0
