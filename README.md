# emcp

An OpenCode-compatible MCP server that drives one or more Lua enviroments over a loopback WebSocket. The MCP side uses `rmcp` over stdio; the bridge uses a small `smol` WebSocket server, and the Lua side uses a `WebSocket` client API.

## Build

```sh
cargo build --release
```

OpenCode starts the built binary as a local stdio MCP server. The project `opencode.jsonc` points at `emcp` (so you need to copy it into your PATH) and starts the WebSocket bridge on `127.0.0.1:8765`.