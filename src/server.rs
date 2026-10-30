use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::{NotificationContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::bridge::{Bridge, ClientSummary};

const ANDROID_PACKAGE: &str = "com.roblox.client";
const ANDROID_LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

const INSTRUCTIONS: &str = "\
emcp drives one or more Roblox executors through runtime.lua over a loopback WebSocket. \
Inject runtime.lua into each executor once; it reconnects automatically and identifies itself with the executor user's username. \
Use `clients` to list connected executors, then pass `client` (the executor username) to `run`, `workspace`, `search`, and `join` when more than one is connected. \
Use `run` for Luau, `workspace` to inspect the Workspace tree, and `search` to find instances held in the garbage collector. \
Use `join` to enter a place by id; it teleports a connected executor, or, when no executor is connected and `am` is on PATH, launches Roblox for Android user 0 and waits up to 30 seconds for the executor to connect. \
Inside executed code, `warn(...)` and `dbg(...)` are forwarded to this MCP client with the originating client attached.";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunParams {
    /// Luau source code to execute in the executor.
    pub code: String,
    /// Optional chunk name used in error tracebacks.
    pub name: Option<String>,
    /// Execution timeout in milliseconds. Defaults to 30000, maximum 300000.
    pub timeout_ms: Option<u64>,
    /// Connected executor username. Omit when exactly one client is connected; use `clients` to list them.
    pub client: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkspaceParams {
    /// How many child levels to include. Defaults to 2, maximum 8.
    pub depth: Option<u32>,
    /// Maximum number of instance nodes to return. Defaults to 200, maximum 1000.
    pub max_nodes: Option<u32>,
    /// Connected executor username. Omit when exactly one client is connected; use `clients` to list them.
    pub client: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Optional instance name filter. Substring matching is used unless `exact` is true. Provide `name` and/or `properties`.
    pub name: Option<String>,
    /// Property filters as a map of property name to expected value. All filters must match.
    /// Values may be a single value or an array of alternatives. `Type` is an alias for `ClassName`.
    /// Property values are compared exactly. For example: { "ClassName": "TextButton", "Text": "Play" }.
    pub properties: Option<BTreeMap<String, Value>>,
    /// Require an exact name match. Defaults to false.
    pub exact: Option<bool>,
    /// Maximum number of matches to return. Defaults to 50, maximum 200.
    pub limit: Option<u32>,
    /// Connected executor username. Omit when exactly one client is connected; use `clients` to list them.
    pub client: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JoinParams {
    /// Roblox place (game) id to join.
    #[schemars(range(min = 1))]
    #[serde(alias = "game_id")]
    pub place_id: u64,
    /// Connected executor username. Omit when exactly one client is connected.
    /// When omitted and no client is connected, the server launches Roblox through Android's `am`
    /// and waits up to 30 seconds for the executor to connect.
    pub client: Option<String>,
}

#[derive(Clone)]
pub struct Emcp {
    bridge: Bridge,
}

#[tool_router]
impl Emcp {
    pub fn new(bridge: Bridge) -> Self {
        Self { bridge }
    }

    #[tool(
        description = "List connected Roblox executor clients with username, executor, and connection time. Use the username as the `client` argument of run, workspace, search, or join.",
        annotations(title = "List Clients", read_only_hint = true, open_world_hint = true)
    )]
    async fn clients(&self) -> Result<CallToolResult, McpError> {
        let clients = self.bridge.clients();
        let events = self.bridge.take_all_events();
        let payload = json!({
            "count": clients.len(),
            "clients": clients,
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(render(
            true, payload, events, None,
        ))]))
    }

    #[tool(
        description = "Execute Luau in a connected Roblox executor. Compile and runtime errors are returned with the full debug.traceback. warn() and dbg() events are returned with the result. If more than one client is connected, pass `client` with the executor username.",
        annotations(
            title = "Run Luau",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn run(
        &self,
        Parameters(params): Parameters<RunParams>,
    ) -> Result<CallToolResult, McpError> {
        let timeout =
            Duration::from_millis(params.timeout_ms.unwrap_or(30_000).clamp(1_000, 300_000));
        self.call_runtime(
            params.client.as_deref(),
            "run",
            json!({
                "code": params.code,
                "name": params.name,
            }),
            timeout,
        )
        .await
    }

    #[tool(
        description = "Return a JSON tree of game.Workspace from a connected Roblox executor, including each instance's name, class, full name, and children. Use depth and max_nodes to bound the result. If more than one client is connected, pass `client` with the executor username.",
        annotations(
            title = "Inspect Workspace",
            read_only_hint = true,
            open_world_hint = true
        )
    )]
    async fn workspace(
        &self,
        Parameters(params): Parameters<WorkspaceParams>,
    ) -> Result<CallToolResult, McpError> {
        self.call_runtime(
            params.client.as_deref(),
            "workspace",
            json!({
                "depth": params.depth.unwrap_or(2).min(8),
                "max_nodes": params.max_nodes.unwrap_or(200).clamp(1, 1000),
            }),
            Duration::from_secs(20),
        )
        .await
    }

    #[tool(
        description = "Search instances in a connected executor's garbage collector (getgc) by instance name and/or multiple properties. `properties` maps property names to expected values (for example, ClassName = 'TextButton' and Text = 'Play'); every property must match exactly, values may be arrays of alternatives, and `Type` is an alias for `ClassName`. Matching scans every GC instance and evaluates every requested property, so calls are O(nr_instances*nr_properties). Each match includes its full name, canonicalized parent, and the matched property values. If more than one client is connected, pass `client` with the executor username.",
        annotations(
            title = "Search Instances",
            read_only_hint = true,
            open_world_hint = true
        )
    )]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = params.client.as_deref();
        let name = params.name.filter(|name| !name.trim().is_empty());
        let properties = params.properties.unwrap_or_default();

        if properties.keys().any(|key| key.trim().is_empty()) {
            return Ok(self.invalid_request(client, "search property names must not be empty"));
        }
        if name.is_none() && properties.is_empty() {
            return Ok(self.invalid_request(
                client,
                "search requires a non-empty name and/or at least one property filter",
            ));
        }

        self.call_runtime(
            client,
            "search",
            json!({
                "name": name,
                "properties": properties,
                "exact": params.exact.unwrap_or(false),
                "limit": params.limit.unwrap_or(50).clamp(1, 200),
            }),
            Duration::from_secs(30),
        )
        .await
    }

    #[tool(
        description = "Join a Roblox place by id. With a connected executor, schedule a TeleportService teleport on the selected client; the executor disconnects when the teleport starts. If no executor is connected, no `client` is given, and Android's `am` is on PATH, launch Roblox via a roblox://experiences/start deep link for Android user 0 and wait up to 30 seconds for an executor to connect. Use `clients` to list connected executors.",
        annotations(
            title = "Join Game",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn join(
        &self,
        Parameters(params): Parameters<JoinParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.place_id == 0 {
            return Ok(self.invalid_request(
                params.client.as_deref(),
                "join: place_id must be a positive integer",
            ));
        }

        let has_selector = params
            .client
            .as_deref()
            .map(str::trim)
            .is_some_and(|selector| !selector.is_empty());
        if !has_selector && !self.bridge.is_connected() {
            return self.launch_android(params.place_id).await;
        }

        self.call_runtime(
            params.client.as_deref(),
            "join",
            json!({ "place_id": params.place_id }),
            Duration::from_secs(15),
        )
        .await
    }

    async fn launch_android(&self, place_id: u64) -> Result<CallToolResult, McpError> {
        let launch = tokio::task::spawn_blocking(move || launch_with_am(place_id)).await;

        let (success, payload, client) = match launch {
            Ok(Ok((command, stdout, stderr))) => {
                let started = Instant::now();
                let client = self.bridge.wait_for_client(ANDROID_LAUNCH_TIMEOUT).await;
                let connected = client.is_some();
                (
                    true,
                    json!({
                        "launched": true,
                        "method": "android-am",
                        "place_id": place_id,
                        "android_user": 0,
                        "package": ANDROID_PACKAGE,
                        "command": command,
                        "stdout": stdout,
                        "stderr": stderr,
                        "waited_ms": started.elapsed().as_millis() as u64,
                        "connected": connected,
                        "clients": self.bridge.clients(),
                        "note": if connected {
                            "Roblox launch requested and the executor connected."
                        } else {
                            "Roblox launch requested, but no executor connected within 30 seconds. Inject runtime.lua after the game loads or enable auto-execute so it can reconnect."
                        },
                    }),
                    client,
                )
            }
            Ok(Err(error)) => (
                false,
                json!({
                    "error": error,
                    "method": "android-am",
                    "place_id": place_id,
                    "clients": self.bridge.clients(),
                }),
                None,
            ),
            Err(error) => (
                false,
                json!({
                    "error": format!("join task failed: {error}"),
                    "method": "android-am",
                    "place_id": place_id,
                    "clients": self.bridge.clients(),
                }),
                None,
            ),
        };
        let events = self.bridge.take_all_events();

        Ok(if success {
            CallToolResult::success(vec![ContentBlock::text(render(
                true,
                payload,
                events,
                client.as_ref(),
            ))])
        } else {
            CallToolResult::error(vec![ContentBlock::text(render(
                false, payload, events, None,
            ))])
        })
    }

    async fn call_runtime(
        &self,
        client: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        let target = match self.bridge.resolve_client(client) {
            Ok(target) => target,
            Err(error) => {
                let events = self.bridge.take_all_events();
                let payload = json!({
                    "error": error,
                    "clients": self.bridge.clients(),
                });
                return Ok(CallToolResult::error(vec![ContentBlock::text(render(
                    false, payload, events, None,
                ))]));
            }
        };

        let mut events = self.bridge.take_events(target.id);
        let result = self
            .bridge
            .execute_for(&target, method, params, timeout)
            .await;
        events.extend(self.bridge.take_events(target.id));

        let (success, payload) = match result {
            Ok(value) => (true, value),
            Err(error) => (false, json!({ "error": error })),
        };

        Ok(if success {
            CallToolResult::success(vec![ContentBlock::text(render(
                true,
                payload,
                events,
                Some(&target),
            ))])
        } else {
            CallToolResult::error(vec![ContentBlock::text(render(
                false,
                payload,
                events,
                Some(&target),
            ))])
        })
    }

    fn invalid_request(&self, client: Option<&str>, message: impl Into<String>) -> CallToolResult {
        let message = message.into();
        let (target, events) = match self.bridge.resolve_client(client) {
            Ok(target) => {
                let events = self.bridge.take_events(target.id);
                (Some(target), events)
            }
            Err(_) => (None, self.bridge.take_all_events()),
        };

        let mut payload = json!({ "error": message });
        if target.is_none() {
            payload["clients"] = serde_json::to_value(self.bridge.clients()).unwrap_or(Value::Null);
        }

        CallToolResult::error(vec![ContentBlock::text(render(
            false,
            payload,
            events,
            target.as_ref(),
        ))])
    }
}

#[tool_handler]
impl ServerHandler for Emcp {
    #[allow(deprecated)]
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_logging()
                .build(),
        )
        .with_server_info(Implementation::new("emcp", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        self.bridge.set_peer(context.peer.clone());
    }
}

fn am_arguments(place_id: u64) -> Vec<String> {
    vec![
        "start".to_string(),
        "--user".to_string(),
        "0".to_string(),
        "-a".to_string(),
        "android.intent.action.VIEW".to_string(),
        "-p".to_string(),
        ANDROID_PACKAGE.to_string(),
        "-d".to_string(),
        format!("roblox://experiences/start?placeId={place_id}"),
    ]
}

fn launch_with_am(place_id: u64) -> Result<(String, String, String), String> {
    const NOT_FOUND: &str = "no executor is connected and `am` was not found in PATH";

    let path = std::env::var_os("PATH").ok_or_else(|| NOT_FOUND.to_string())?;
    let executable = std::env::split_paths(&path)
        .map(|directory| directory.join("am"))
        .find(|candidate| {
            let Ok(metadata) = candidate.metadata() else {
                return false;
            };
            if !metadata.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
        .ok_or_else(|| NOT_FOUND.to_string())?;

    let arguments = am_arguments(place_id);
    let command = format!("{} {}", executable.display(), arguments.join(" "));
    let output = Command::new(&executable)
        .args(&arguments)
        .output()
        .map_err(|error| format!("failed to run `{}`: {error}", executable.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let detail = [stdout.as_str(), stderr.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(if detail.is_empty() {
            format!("`am start` failed with {}", output.status)
        } else {
            format!("`am start` failed with {}: {detail}", output.status)
        });
    }

    Ok((command, stdout, stderr))
}

fn render(
    success: bool,
    payload: Value,
    events: Vec<Value>,
    client: Option<&ClientSummary>,
) -> String {
    let mut object = match payload {
        Value::Object(object) => object,
        other => {
            let mut object = Map::new();
            object.insert("result".to_string(), other);
            object
        }
    };
    if let Some(client) = client {
        object.insert(
            "client".to_string(),
            serde_json::to_value(client).unwrap_or(Value::Null),
        );
    }
    object.insert("success".to_string(), Value::Bool(success));
    object.insert("events".to_string(), Value::Array(events));

    serde_json::to_string_pretty(&Value::Object(object))
        .unwrap_or_else(|error| format!("{{\"success\":false,\"error\":\"{error}\"}}"))
}

pub async fn serve(bridge: Bridge) -> anyhow::Result<()> {
    let service = Emcp::new(bridge).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::am_arguments;

    #[test]
    fn android_launch_targets_user_zero_and_the_roblox_deep_link() {
        assert_eq!(
            am_arguments(1818).join(" "),
            "start --user 0 -a android.intent.action.VIEW -p com.roblox.client -d roblox://experiences/start?placeId=1818"
        );
    }
}
