# harness

`harness` is a minimal ACP backend that combines:

- an ACP agent implementation
- session and mode management
- an OpenAI-compatible LLM client
- per-session tool runtimes
- MCP-backed tool sources
- a hardened Python execution path for `execute`
- Cedar-based tool authorization

The guiding constraint is that the ACP agent layer stays as stateless as possible. Session meaning lives in the session manager and store, while live tool processes are owned by the runtime layer.

## Architecture

```text
ACP client
   |
   v
HarnessAgent (thin ACP shim)
   |
   +-> SessionManager
   |     +-> SessionStore
   |     \-> ToolRuntimeManager
   |
   +-> LLM provider
   +-> Tool policy engine
   \-> Python sandbox runner
```

## What Works Today

- ACP session lifecycle, including load, list, fork, resume, close, mode changes, and model changes
- Built-in session modes: `code`, `ask`, and `architect`
- Per-mode tool source selection through `mode_tool_sources`
- Per-session model switching from a configured `available_models` list
- MCP-backed tool discovery and execution
- A Python `execute` path that can call tools through the harness
- Per-request principal handling for tool authorization
- Cancellation across LLM calls and sandbox execution

## Quick Start

Build the harness:

```sh
cargo build --release
```

Run the local-model example:

```sh
./target/release/harness --config examples/local.json
```

Run the MCP-enabled example:

```sh
./target/release/harness --config examples/with_mcp.json
```

The example configs target an OpenAI-compatible endpoint at `http://localhost:11434/v1` and default to `llama3.1`.

## Configuration

The config file can be JSON or YAML. At a high level it looks like this:

```json
{
  "name": "local-agent",
  "system_prompt": "You are a helpful coding assistant.",
  "model": {
    "model_id": "llama3.1",
    "available_models": ["llama3.1"],
    "base_url": "http://localhost:11434/v1",
    "api_key": null,
    "max_tokens": 1024,
    "temperature": 0.2
  },
  "mcp_servers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "env": {}
    }
  },
  "mode_tool_sources": {
    "architect": []
  },
  "tool_policy": "permit(principal, action, resource);"
}
```

Key ideas:

- `mcp_servers` defines named tool sources
- `mode_tool_sources` narrows which tool sources are active in each mode
- `tool_policy` is a Cedar policy evaluated for tool access

## Sandbox Story

The harness includes a hardened Python execution path for `execute`. It is much safer than the original in-process design, but it is still a process sandbox, not a VM or container boundary.

Today it provides:

- separate Python process execution
- restricted imports and builtins
- bounded stdout, stderr, tool payloads, and IPC
- execution timeouts and cancellation
- resource limits and a temporary working directory

That is good enough for controlled tool execution, but it is not the end state for hostile multi-tenant isolation.

## Important Limits

- The default binary uses an in-memory session store, so load, list, fork, close, and resume behavior only lasts for the lifetime of the process.
- There is no durable off-process session history layer yet.
- Conversation compaction and summarization are not implemented yet.
- LLM responses are returned as whole messages, not streamed token by token.
- MCP health recovery is still basic; failed servers are not automatically rebuilt into a healthy topology.

## Examples

- [`examples/local.json`](examples/local.json): local model, no MCP tools
- [`examples/with_mcp.json`](examples/with_mcp.json): filesystem MCP server, with `architect` mode configured to run tool-free

## Development

```sh
cargo test
cargo clippy --all-targets --all-features -- -D warnings
python3 -m unittest sandbox/test_runner.py
```

## License

Apache-2.0
