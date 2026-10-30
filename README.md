# emcp

An OpenCode-compatible MCP server that drives one or more Roblox executors over
a loopback WebSocket. The MCP side uses `rmcp` over stdio; the bridge uses a
small `smol` WebSocket server, and the executor side uses the sUNC `WebSocket`
client API.

## Build

```sh
cargo build --release
```

OpenCode starts the built binary as a local stdio MCP server. The project
`opencode.jsonc` points at `./target/release/emcp` and starts the WebSocket
bridge on `127.0.0.1:8765`.

## Connect an executor

Execute `runtime.lua` in each executor. It connects to
`ws://127.0.0.1:8765`, sends a hello message containing the executor user's
username, and reconnects when the MCP server restarts. The username is
`Players.LocalPlayer.Name`; override it before running the runtime if needed:

```lua
getgenv().EMCP_USERNAME = "alice"
```

If the bridge uses another address, set it before running the runtime:

```lua
getgenv().EMCP_URL = "ws://127.0.0.1:9000"
```

and start the server with the matching address:

```sh
EMCP_ADDR=127.0.0.1:9000 ./target/release/emcp
```

Multiple executors can stay connected at the same time. Each connection is
identified by its executor username; the same account cannot be connected from
two devices at once.

## Tools

- `clients` lists connected executors with username, executor, and connection
  time.
- `run` executes Luau with `loadstring` and `xpcall`. A compile or runtime error
  returns the complete `debug.traceback` to the model. Return values are
  serialized to readable strings.
- `workspace` returns a bounded JSON tree of `game.Workspace`.
- `join` joins a Roblox place by id. A connected executor schedules a
  `TeleportService:Teleport`, which disconnects the executor when the teleport
  starts. If no executor is connected and no `client` is given, the server looks
  for `am` on `PATH` and launches Roblox with
  `am start --user 0 ... roblox://experiences/start?placeId=<id>` so the primary
  Android user profile is used. After launching, `join` waits up to 30 seconds
  for an executor to connect and send its hello before returning.
- `search` scans `getgc(false)` for instances matching an optional `Name`
  filter and/or a `properties` object. Every property filter must match exactly;
  values can be arrays of alternatives, and `Type` is an alias for `ClassName`.
  Each result includes the instance's full name, canonicalized parent, and
  matched property values. Matching evaluates every GC instance against every
  requested property, so calls are `O(nr_instances*nr_properties)`.

`run`, `workspace`, `search`, and `join` accept `client` (the executor
username). Omit it when exactly one executor is connected; with multiple
executors, omitting it returns an error listing the available usernames.

`warn(...)` inside executed code is forwarded to the MCP server, and `dbg(...)`
sends its text without printing. Both are emitted as MCP logging notifications
and attached to the next tool result for the originating client, so the model
receives them even when the client ignores notifications.

## Protocol

The runtime sends a hello when it connects:

```json
{"type":"hello","name":"emcp-runtime","version":1,"username":"alice","executor":"..."}
```

The MCP server sends JSON requests over the WebSocket:

```json
{"type":"request","id":"1","method":"run","params":{"code":"return 1"}}
{"type":"request","id":"2","method":"join","params":{"place_id":1818}}
{"type":"request","id":"3","method":"search","params":{"name":null,"properties":{"ClassName":"TextButton","Text":"Play"},"exact":false,"limit":50}}
```

The runtime answers with a response or an event:

```json
{"type":"response","id":"1","ok":true,"result":{"duration_ms":1,"results":["1"]}}
{"type":"event","event":"warn","text":"careful","traceback":"stack traceback: ..."}
```

## Tests

```sh
cargo test
luajit tests/runtime_spec.lua runtime.lua
```

The Rust tests run minimal `smol` WebSocket executors against the bridge and
verify username identification, waiting for a client to identify itself, request
routing, ambiguity errors, and per-client event attribution. The Lua test mocks the Roblox globals and
verifies traceback forwarding, `warn`, `dbg`, username hello, workspace
traversal, name search, property search, join teleports, and `getgc` parent
canonicalization. Android launches use `--user 0`, Roblox's
`roblox://experiences/start?placeId=...` deep link, and the
`com.roblox.client` package.

The bridge binds to loopback only. It executes arbitrary Luau in every connected
executor, so do not expose the port.
