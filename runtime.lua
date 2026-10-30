-- emcp executor runtime
-- Injects a WebSocket client that executes run/workspace/search requests and
-- forwards warn/dbg output to the emcp MCP server.

local ok_env, env = pcall(function()
    return getgenv and getgenv()
end)
local genv = ok_env and type(env) == "table" and env or _G

local previous = genv.__EMCP_RUNTIME__
if previous and previous.stop then
    pcall(previous.stop)
end

local HttpService = game:GetService("HttpService")
local Workspace = game:GetService("Workspace")
local URL = genv.EMCP_URL or "ws://127.0.0.1:8765"

local runtime = {}
genv.__EMCP_RUNTIME__ = runtime

local socket
local running = true

local function safe_tostring(value)
    local ok, text = pcall(tostring, value)
    if ok then
        return text
    end
    return "<unprintable>"
end

local function safe_typeof(value)
    local ok, kind = pcall(typeof, value)
    if ok then
        return kind
    end
    return type(value)
end

local function describe(value, depth, seen)
    depth = depth or 0
    seen = seen or {}

    local kind = safe_typeof(value)
    if kind == "Instance" then
        local ok, name = pcall(function()
            return value:GetFullName()
        end)
        if ok and type(name) == "string" then
            return name
        end
        local ok_name, simple = pcall(function()
            return value.Name
        end)
        if ok_name then
            return "<Instance " .. safe_tostring(simple) .. ">"
        end
        return "<Instance>"
    end

    if kind == "string" then
        local escaped = value:gsub("\\", "\\\\"):gsub('"', '\\"'):gsub("\n", "\\n"):gsub("\r", "\\r"):gsub("\t", "\\t")
        return '"' .. escaped .. '"'
    end

    if kind == "number" or kind == "boolean" or kind == "nil" then
        return safe_tostring(value)
    end

    if kind ~= "table" then
        return safe_tostring(value)
    end

    if seen[value] then
        return "<cycle>"
    end
    if depth >= 3 then
        return "{...}"
    end

    seen[value] = true
    local parts = {}
    local count = 0
    local ok = pcall(function()
        for key, item in pairs(value) do
            if count >= 25 then
                parts[#parts + 1] = "..."
                break
            end
            count = count + 1
            local key_text
            if type(key) == "string" then
                key_text = key
            else
                key_text = "[" .. describe(key, depth + 1, seen) .. "]"
            end
            parts[#parts + 1] = key_text .. " = " .. describe(item, depth + 1, seen)
        end
    end)
    seen[value] = nil

    if not ok then
        return "<table>"
    end
    return "{" .. table.concat(parts, ", ") .. "}"
end

local function format_join(...)
    local parts = {}
    for index = 1, select("#", ...) do
        local value = select(index, ...)
        if type(value) == "string" then
            parts[#parts + 1] = value
        else
            parts[#parts + 1] = describe(value, 0, {})
        end
    end
    return table.concat(parts, " ")
end

local MAX_EVENT_TEXT = 4096

local function limited(text, limit)
    if type(text) ~= "string" or #text <= limit then
        return text
    end
    return text:sub(1, limit) .. "... [truncated]"
end

local function capture_traceback(level)
    local ok, trace = pcall(debug.traceback, "", level or 2)
    if ok and type(trace) == "string" then
        return trace
    end
    return nil
end

local function encode(message)
    local ok, text = pcall(function()
        return HttpService:JSONEncode(message)
    end)
    if ok and type(text) == "string" then
        return text
    end
    return nil
end

local function send(message)
    if not socket then
        return false
    end
    local text = encode(message)
    if not text then
        return false
    end
    local ok = pcall(function()
        socket:Send(text)
    end)
    if not ok then
        socket = nil
    end
    return ok
end

local function emit(kind, text, extra)
    local message = {
        type = "event",
        event = kind,
        text = text,
    }
    if extra then
        for key, value in pairs(extra) do
            message[key] = value
        end
    end
    send(message)
end

local function sleep(seconds)
    if task and task.wait then
        task.wait(seconds)
    elseif wait then
        wait(seconds)
    end
end

local function spawn(fn)
    if task and task.spawn then
        return task.spawn(fn)
    end
    local thread = coroutine.create(fn)
    local ok, err = coroutine.resume(thread)
    if not ok then
        emit("protocol_error", "runtime task failed: " .. safe_tostring(err))
    end
    return thread
end

local function respond(id, ok, payload, extra)
    local message = {
        type = "response",
        id = id,
        ok = ok,
    }
    if ok then
        message.result = payload
    else
        message.error = payload
    end
    if extra then
        for key, value in pairs(extra) do
            message[key] = value
        end
    end
    send(message)
end

local function pack(...)
    return { n = select("#", ...), ... }
end

local original_warn = warn

local function make_run_environment()
    local environment = setmetatable({}, {
        __index = genv,
    })
    environment._G = environment
    environment.warn = function(...)
        pcall(runtime.emit_warn, ...)
        if type(original_warn) == "function" then
            return original_warn(...)
        end
    end
    environment.dbg = function(...)
        pcall(runtime.emit_dbg, ...)
    end
    return environment
end

local function execute_run(id, params)
    local code = params.code
    if type(code) ~= "string" then
        respond(id, false, "run: code must be a string")
        return
    end

    local loader = loadstring or load
    if type(loader) ~= "function" then
        respond(id, false, "run: loadstring/load is unavailable\n" .. (capture_traceback(2) or ""))
        return
    end

    local chunk_name = params.name or "=emcp"
    local fn, compile_error = loader(code, chunk_name)
    if type(fn) ~= "function" then
        respond(id, false, "compile error:\n" .. (capture_traceback(2) or "") .. "\n" .. safe_tostring(compile_error))
        return
    end

    if type(setfenv) ~= "function" then
        respond(id, false, "run: setfenv is unavailable; cannot scope the execution environment")
        return
    end
    local env_ok, env_error = pcall(setfenv, fn, make_run_environment())
    if not env_ok then
        respond(id, false, "run: setfenv failed: " .. safe_tostring(env_error))
        return
    end

    local started = os.clock()
    spawn(function()
        local results = pack(xpcall(fn, function(err)
            return debug.traceback(safe_tostring(err), 2)
        end))
        local duration = math.floor((os.clock() - started) * 1000)

        if not results[1] then
            respond(id, false, "runtime error:\n" .. safe_tostring(results[2]), {
                duration_ms = duration,
            })
            return
        end

        local values = {}
        for index = 2, results.n do
            values[#values + 1] = describe(results[index], 0, {})
        end
        respond(id, true, {
            duration_ms = duration,
            results = values,
        })
    end)
end

local function instance_name(instance)
    local ok, name = pcall(function()
        return instance.Name
    end)
    if ok then
        return safe_tostring(name)
    end
    return "<unknown>"
end

local function class_name(instance)
    local ok, name = pcall(function()
        return instance.ClassName
    end)
    if ok then
        return safe_tostring(name)
    end
    return "<unknown>"
end

local function full_name(instance)
    local ok, name = pcall(function()
        return instance:GetFullName()
    end)
    if ok and type(name) == "string" then
        return name
    end
    return instance_name(instance)
end

local function parent_summary(instance)
    local ok, parent = pcall(function()
        return instance.Parent
    end)
    if not ok or not parent then
        return nil
    end
    return {
        name = instance_name(parent),
        class_name = class_name(parent),
        full_name = full_name(parent),
    }
end

local function children_of(instance)
    local ok, children = pcall(function()
        return instance:GetChildren()
    end)
    if ok and type(children) == "table" then
        return children
    end
    return {}
end

local function serialize_tree(instance, depth, budget, visited)
    if budget.count >= budget.max_nodes then
        budget.truncated = true
        return nil
    end
    if visited[instance] then
        return nil
    end

    visited[instance] = true
    budget.count = budget.count + 1

    local children = children_of(instance)
    local node = {
        name = instance_name(instance),
        class_name = class_name(instance),
        full_name = full_name(instance),
        child_count = #children,
    }

    if depth > 0 and #children > 0 then
        node.children = {}
        for _, child in ipairs(children) do
            if budget.count >= budget.max_nodes then
                budget.truncated = true
                break
            end
            local child_node = serialize_tree(child, depth - 1, budget, visited)
            if child_node then
                node.children[#node.children + 1] = child_node
            end
        end
    end

    visited[instance] = nil
    return node
end

local function execute_workspace(id, params)
    local depth = math.max(0, math.min(tonumber(params.depth) or 2, 8))
    local max_nodes = math.max(1, math.min(tonumber(params.max_nodes) or 200, 1000))
    local budget = {
        count = 0,
        max_nodes = max_nodes,
        truncated = false,
    }

    local root = serialize_tree(Workspace, depth, budget, {})
    respond(id, true, {
        root = root,
        depth = depth,
        max_nodes = max_nodes,
        nodes = budget.count,
        truncated = budget.truncated,
    })
end

local function execute_join(id, params)
    local place_id = tonumber(params.place_id)
    if not place_id or place_id <= 0 or place_id % 1 ~= 0 then
        respond(id, false, "join: place_id must be a positive integer")
        return
    end

    local ok_service, teleport_service = pcall(function()
        return game:GetService("TeleportService")
    end)
    if not ok_service or not teleport_service then
        respond(id, false, "join: TeleportService is unavailable\n" .. (capture_traceback(2) or ""))
        return
    end

    local ok_teleport, teleport = pcall(function()
        return teleport_service.Teleport
    end)
    if not ok_teleport or type(teleport) ~= "function" then
        respond(id, false, "join: TeleportService:Teleport is unavailable")
        return
    end

    respond(id, true, {
        place_id = place_id,
        scheduled = true,
    })

    spawn(function()
        local ok, err = pcall(teleport, teleport_service, place_id)
        if not ok then
            emit("protocol_error", "join: teleport failed: " .. safe_tostring(err), {
                place_id = place_id,
            })
        end
    end)
end

local function is_array(value)
    if type(value) ~= "table" then
        return false
    end
    local count = 0
    for key in pairs(value) do
        if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then
            return false
        end
        count = count + 1
    end
    return count > 0 and count == #value
end

local function canonical_property(name)
    if name == "Type" or name == "type" then
        return "ClassName"
    end
    return name
end

local function export_value(value)
    local kind = type(value)
    if kind == "string" or kind == "number" or kind == "boolean" then
        return value
    end
    return describe(value, 0, {})
end

local function property_matches(actual, expected)
    if type(expected) == "table" and is_array(expected) then
        for _, candidate in ipairs(expected) do
            if property_matches(actual, candidate) then
                return true
            end
        end
        return false
    end
    if type(expected) == "string" then
        if type(actual) == "string" then
            return actual == expected
        end
        return safe_tostring(actual) == expected
    end
    return actual == expected
end

local function execute_search(id, params)
    local query = params.name
    if query ~= nil and type(query) ~= "string" then
        respond(id, false, "search: name must be a string")
        return
    end
    local has_name = type(query) == "string" and query ~= ""

    local filters = params.properties
    if filters ~= nil and type(filters) ~= "table" then
        respond(id, false, "search: properties must be an object")
        return
    end
    if type(filters) ~= "table" then
        filters = {}
    end

    local filter_count = 0
    for property in pairs(filters) do
        if type(property) ~= "string" or property == "" then
            respond(id, false, "search: property names must be non-empty strings")
            return
        end
        filter_count = filter_count + 1
    end
    if not has_name and filter_count == 0 then
        respond(id, false, "search: provide a name and/or at least one property filter")
        return
    end

    local exact = params.exact == true
    local limit = math.max(1, math.min(tonumber(params.limit) or 50, 200))
    local lowered = has_name and string.lower(query) or nil

    local ok_gc, gc = pcall(getgc, false)
    if not ok_gc then
        respond(id, false, "getgc failed:\n" .. (capture_traceback(2) or "") .. "\n" .. safe_tostring(gc))
        return
    end
    if type(gc) ~= "table" then
        respond(id, false, "getgc returned " .. safe_typeof(gc) .. " instead of a table")
        return
    end

    local matches = {}
    local seen = {}
    local matched = 0
    local scanned = 0

    for index = 1, #gc do
        local object = gc[index]
        if safe_typeof(object) == "Instance" then
            scanned = scanned + 1
            local ok_name, name = pcall(function()
                return object.Name
            end)
            if ok_name and type(name) == "string" then
                local name_matches
                if has_name then
                    if exact then
                        name_matches = name == query
                    else
                        name_matches = string.find(string.lower(name), lowered, 1, true) ~= nil
                    end
                else
                    name_matches = true
                end

                if name_matches then
                    local matched_properties = {}
                    local properties_match = true
                    for property, expected in pairs(filters) do
                        local ok_property, actual = pcall(function()
                            return object[canonical_property(property)]
                        end)
                        if not ok_property or not property_matches(actual, expected) then
                            properties_match = false
                            break
                        end
                        matched_properties[property] = export_value(actual)
                    end

                    if properties_match then
                        local instance_full_name = full_name(object)
                        local instance_class = class_name(object)
                        local key = instance_full_name .. "\0" .. instance_class
                        if not seen[key] then
                            seen[key] = true
                            matched = matched + 1
                            if #matches < limit then
                                local parent = parent_summary(object)
                                local match = {
                                    name = name,
                                    class_name = instance_class,
                                    full_name = instance_full_name,
                                    parent = parent,
                                    parent_full_name = parent and parent.full_name or nil,
                                }
                                if filter_count > 0 then
                                    match.properties = matched_properties
                                end
                                matches[#matches + 1] = match
                            end
                        end
                    end
                end
            end
        end
    end

    table.sort(matches, function(left, right)
        return left.full_name < right.full_name
    end)

    respond(id, true, {
        query = has_name and query or nil,
        exact = exact,
        property_filters = filter_count,
        scanned_instances = scanned,
        matched = matched,
        returned = #matches,
        truncated = matched > #matches,
        matches = matches,
    })
end

local function dispatch(message)
    local id = message.id
    local method = message.method
    if id == nil or type(method) ~= "string" then
        emit("protocol_error", "request is missing id or method")
        return
    end

    local params = message.params
    if type(params) ~= "table" then
        params = {}
    end

    local ok, err = xpcall(function()
        if method == "run" then
            execute_run(id, params)
        elseif method == "workspace" then
            execute_workspace(id, params)
        elseif method == "search" then
            execute_search(id, params)
        elseif method == "join" then
            execute_join(id, params)
        else
            respond(id, false, "unknown method: " .. safe_tostring(method))
        end
    end, function(error_value)
        return debug.traceback(safe_tostring(error_value), 2)
    end)

    if not ok then
        respond(id, false, "dispatch error:\n" .. safe_tostring(err))
    end
end

local function executor_identity()
    local ok, name, version = pcall(identifyexecutor)
    if ok and name then
        local text = safe_tostring(name)
        if version then
            text = text .. " " .. safe_tostring(version)
        end
        return text
    end
    return "unknown"
end

local function executor_username()
    local override = genv.EMCP_USERNAME
    if type(override) == "string" and override ~= "" then
        return override
    end

    local ok_players, players = pcall(function()
        return game:GetService("Players")
    end)
    if ok_players and players then
        local ok_player, player = pcall(function()
            return players.LocalPlayer
        end)
        if ok_player and player then
            local ok_name, name = pcall(function()
                return player.Name
            end)
            if ok_name and type(name) == "string" and name ~= "" then
                return name
            end
        end
    end

    return "unknown"
end

local function connect_loop()
    local attempt = 0

    while running do
        local connected, ws = pcall(WebSocket.connect, URL)
        if connected and ws then
            socket = ws
            attempt = 0

            if ws.OnMessage and ws.OnMessage.Connect then
                ws.OnMessage:Connect(function(message)
                    if type(message) ~= "string" then
                        emit("protocol_error", "received a non-string websocket message")
                        return
                    end
                    spawn(function()
                        local ok, decoded = pcall(function()
                            return HttpService:JSONDecode(message)
                        end)
                        if not ok then
                            emit("protocol_error", "invalid JSON from server: " .. safe_tostring(decoded))
                            return
                        end
                        if type(decoded) ~= "table" then
                            emit("protocol_error", "server message is not a JSON object")
                            return
                        end
                        dispatch(decoded)
                    end)
                end)
            end

            if ws.OnClose and ws.OnClose.Connect then
                ws.OnClose:Connect(function()
                    if socket == ws then
                        socket = nil
                    end
                end)
            end

            send({
                type = "hello",
                name = "emcp-runtime",
                version = 1,
                username = executor_username(),
                executor = executor_identity(),
            })
            print("[emcp] connected to " .. URL)

            while running and socket == ws do
                sleep(0.25)
            end

            if socket == ws then
                socket = nil
            end
            pcall(function()
                ws:Close()
            end)
        else
            attempt = attempt + 1
            if attempt == 1 or attempt % 10 == 0 then
                print("[emcp] waiting for " .. URL .. " (" .. safe_tostring(ws) .. ")")
            end
            sleep(1)
        end
    end
end

runtime.emit_warn = function(...)
    emit("warn", limited(format_join(...), MAX_EVENT_TEXT), {
        traceback = limited(capture_traceback(3), MAX_EVENT_TEXT),
    })
end

runtime.emit_dbg = function(...)
    emit("dbg", limited(format_join(...), MAX_EVENT_TEXT))
end

runtime.stop = function()
    running = false
    local ws = socket
    socket = nil
    if ws then
        pcall(function()
            ws:Close()
        end)
    end
end

spawn(connect_loop)
