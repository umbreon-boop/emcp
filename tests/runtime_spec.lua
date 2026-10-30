-- Behavioral checks for runtime.lua using a small Roblox/executor mock.
-- Run with: luajit tests/runtime_spec.lua

local function json_encode(value)
    local kind = type(value)
    if value == nil then
        return "null"
    end
    if kind == "boolean" or kind == "number" then
        return tostring(value)
    end
    if kind == "string" then
        local escapes = {
            ['"'] = '\\"',
            ["\\"] = "\\\\",
            ["\b"] = "\\b",
            ["\f"] = "\\f",
            ["\n"] = "\\n",
            ["\r"] = "\\r",
            ["\t"] = "\\t",
        }
        return '"' .. value:gsub('[%z\1-\31\\"]', function(character)
            return escapes[character] or string.format("\\u%04x", string.byte(character))
        end) .. '"'
    end
    if kind ~= "table" then
        return json_encode(tostring(value))
    end

    local is_array = true
    local count = 0
    for key in pairs(value) do
        if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then
            is_array = false
            break
        end
        if key > count then
            count = key
        end
    end

    if is_array and count == #value then
        local parts = {}
        for index = 1, count do
            parts[#parts + 1] = json_encode(value[index])
        end
        return "[" .. table.concat(parts, ",") .. "]"
    end

    local parts = {}
    for key, item in pairs(value) do
        if item ~= nil then
            parts[#parts + 1] = json_encode(tostring(key)) .. ":" .. json_encode(item)
        end
    end
    return "{" .. table.concat(parts, ",") .. "}"
end

local function json_decode(input)
    local position = 1

    local function skip_space()
        while true do
            local character = input:sub(position, position)
            if character == " " or character == "\n" or character == "\r" or character == "\t" then
                position = position + 1
            else
                return
            end
        end
    end

    local parse_value

    local function parse_string()
        position = position + 1
        local output = {}
        while position <= #input do
            local character = input:sub(position, position)
            if character == '"' then
                position = position + 1
                return table.concat(output)
            end
            if character == "\\" then
                local escape = input:sub(position + 1, position + 1)
                if escape == "u" then
                    local code = tonumber(input:sub(position + 2, position + 5), 16)
                    position = position + 6
                    if code < 0x80 then
                        output[#output + 1] = string.char(code)
                    elseif code < 0x800 then
                        output[#output + 1] = string.char(
                            0xC0 + math.floor(code / 0x40),
                            0x80 + code % 0x40
                        )
                    else
                        output[#output + 1] = string.char(
                            0xE0 + math.floor(code / 0x1000),
                            0x80 + math.floor(code / 0x40) % 0x40,
                            0x80 + code % 0x40
                        )
                    end
                else
                    local escapes = {
                        ['"'] = '"',
                        ["\\"] = "\\",
                        ["/"] = "/",
                        b = "\b",
                        f = "\f",
                        n = "\n",
                        r = "\r",
                        t = "\t",
                    }
                    output[#output + 1] = escapes[escape] or escape
                    position = position + 2
                end
            else
                output[#output + 1] = character
                position = position + 1
            end
        end
        error("unterminated JSON string")
    end

    local function parse_number()
        local start = position
        if input:sub(position, position) == "-" then
            position = position + 1
        end
        while input:sub(position, position):match("%d") do
            position = position + 1
        end
        if input:sub(position, position) == "." then
            position = position + 1
            while input:sub(position, position):match("%d") do
                position = position + 1
            end
        end
        local exponent = input:sub(position, position)
        if exponent == "e" or exponent == "E" then
            position = position + 1
            local sign = input:sub(position, position)
            if sign == "+" or sign == "-" then
                position = position + 1
            end
            while input:sub(position, position):match("%d") do
                position = position + 1
            end
        end
        return tonumber(input:sub(start, position - 1))
    end

    parse_value = function()
        skip_space()
        local character = input:sub(position, position)
        if character == '"' then
            return parse_string()
        end
        if character == "{" then
            position = position + 1
            local object = {}
            skip_space()
            if input:sub(position, position) == "}" then
                position = position + 1
                return object
            end
            while true do
                skip_space()
                local key = parse_string()
                skip_space()
                assert(input:sub(position, position) == ":", "expected ':'")
                position = position + 1
                object[key] = parse_value()
                skip_space()
                local delimiter = input:sub(position, position)
                position = position + 1
                if delimiter == "}" then
                    return object
                end
                assert(delimiter == ",", "expected ',' or '}'")
            end
        end
        if character == "[" then
            position = position + 1
            local array = {}
            skip_space()
            if input:sub(position, position) == "]" then
                position = position + 1
                return array
            end
            while true do
                array[#array + 1] = parse_value()
                skip_space()
                local delimiter = input:sub(position, position)
                position = position + 1
                if delimiter == "]" then
                    return array
                end
                assert(delimiter == ",", "expected ',' or ']'")
            end
        end
        if input:sub(position, position + 3) == "true" then
            position = position + 4
            return true
        end
        if input:sub(position, position + 4) == "false" then
            position = position + 5
            return false
        end
        if input:sub(position, position + 3) == "null" then
            position = position + 4
            return nil
        end
        return parse_number()
    end

    return parse_value()
end

local sent = {}
local warnings = {}
local printed = {}
local connected_url

local function new_signal()
    local signal = { callbacks = {} }
    function signal:Connect(callback)
        self.callbacks[#self.callbacks + 1] = callback
    end
    return signal
end

local function new_instance(name, class_name, parent)
    local instance = {
        Name = name,
        ClassName = class_name,
        Parent = parent,
        Children = {},
        __typeof = "Instance",
    }
    function instance:GetChildren()
        return self.Children
    end
    function instance:GetFullName()
        local parts = {}
        local node = self
        while node do
            table.insert(parts, 1, node.Name)
            node = node.Parent
        end
        return table.concat(parts, ".")
    end
    if parent then
        parent.Children[#parent.Children + 1] = instance
    end
    return instance
end

local workspace = new_instance("Workspace", "Workspace", nil)
local part = new_instance("TargetPart", "Part", workspace)
local model = new_instance("Model", "Model", workspace)
local nested = new_instance("NestedPart", "Part", model)
local detached = new_instance("DetachedPart", "Part", nil)
local screen_gui = new_instance("ScreenGui", "ScreenGui", nil)
local play_button = new_instance("PlayButton", "TextButton", screen_gui)
play_button.Text = "Play"
local close_button = new_instance("CloseButton", "TextButton", screen_gui)
close_button.Text = "Close"
local play_label = new_instance("PlayLabel", "TextLabel", screen_gui)
play_label.Text = "Play"

local teleport_calls = {}
local next_teleport_error

local services = {
    Workspace = workspace,
    Players = {
        LocalPlayer = {
            Name = "TestUser",
        },
    },
    TeleportService = {
        Teleport = function(_, place_id)
            if next_teleport_error then
                local message = next_teleport_error
                next_teleport_error = nil
                error(message)
            end
            teleport_calls[#teleport_calls + 1] = place_id
        end,
    },
    HttpService = {
        JSONEncode = function(_, value)
            return json_encode(value)
        end,
        JSONDecode = function(_, value)
            return json_decode(value)
        end,
    },
}

game = {
    GetService = function(_, name)
        return services[name]
    end,
}

typeof = function(value)
    if type(value) == "table" and rawget(value, "__typeof") then
        return rawget(value, "__typeof")
    end
    return type(value)
end

local runtime_genv = setmetatable({}, { __index = _G })
getgenv = function()
    return runtime_genv
end

getgc = function()
    return {
        workspace,
        part,
        model,
        nested,
        detached,
        screen_gui,
        play_button,
        close_button,
        play_label,
        {},
        "not an instance",
    }
end

identifyexecutor = function()
    return "MockExecutor", "1.0"
end

warn = function(...)
    warnings[#warnings + 1] = table.concat({ ... }, " ")
end

print = function(...)
    printed[#printed + 1] = table.concat({ ... }, " ")
end

task = {
    spawn = function(fn, ...)
        local thread = coroutine.create(fn)
        local ok, error_value = coroutine.resume(thread, ...)
        if not ok then
            error(error_value)
        end
        return thread
    end,
    wait = function()
        coroutine.yield()
    end,
}

local socket = {
    OnMessage = new_signal(),
    OnClose = new_signal(),
    Send = function(self, message)
        sent[#sent + 1] = message
    end,
    Close = function(self)
        self.closed = true
        for _, callback in ipairs(self.OnClose.callbacks) do
            callback()
        end
    end,
}

WebSocket = {
    connect = function(url)
        connected_url = url
        return socket
    end,
}

local runtime_path = arg[1] or "runtime.lua"
dofile(runtime_path)

assert(runtime_genv.__EMCP_RUNTIME__ ~= nil, "runtime state is missing from getgenv")
assert(runtime_genv.dbg == nil, "runtime installed dbg in getgenv")
assert(_G.dbg == nil, "runtime leaked dbg into _G")
assert(_G.__EMCP_RUNTIME__ == nil, "runtime leaked state into _G")
assert(_G.__EMCP_WARN_HOOKED__ == nil, "runtime leaked hook state into _G")
assert(connected_url == "ws://127.0.0.1:8765", "unexpected websocket URL: " .. tostring(connected_url))
assert(#sent >= 1, "runtime did not send a hello message")
local hello = json_decode(sent[1])
assert(hello.type == "hello")
assert(hello.username == "TestUser", "hello username was " .. tostring(hello.username))
assert(hello.executor == "MockExecutor 1.0")
assert(#socket.OnMessage.callbacks == 1, "runtime did not subscribe to messages")

local function request(id, method, params)
    local before = #sent
    socket.OnMessage.callbacks[1](json_encode({
        id = id,
        method = method,
        params = params or {},
    }))
    assert(#sent > before, "runtime did not send a response")
    return json_decode(sent[#sent]), before
end

local response = request("run-ok", "run", {
    code = "return 1, 'two'",
    name = "=test",
})
assert(response.ok == true, "successful run was not ok")
assert(response.result.results[1] == "1")
assert(response.result.results[2] == [["two"]])

response = request("run-error", "run", {
    code = "error('boom')",
})
assert(response.ok == false, "failing run was not an error")
assert(response.error:find("boom", 1, true), "error message is missing boom")
assert(response.error:find("stack traceback", 1, true), "error message is missing a traceback")

local run_warn_response, before_run_warn = request("run-warn", "run", {
    code = "warn('from run'); return 7",
})
assert(run_warn_response.ok == true)
local warn_event
for index = before_run_warn + 1, #sent do
    local candidate = json_decode(sent[index])
    if candidate.type == "event" and candidate.event == "warn" then
        warn_event = candidate
        break
    end
end
assert(warn_event and warn_event.text == "from run", "warn inside run was not forwarded")
assert(warn_event.traceback and warn_event.traceback:find("stack traceback", 1, true))

local before_warn = #sent
warn("outside", 42)
assert(#sent == before_warn, "global warn should not be forwarded")
assert(#warnings > 0 and warnings[#warnings] == "outside 42", "original warn was not preserved")

local large_text = string.rep("x", 10000)
local large_response, before_large_warn = request("run-large-warn", "run", {
    code = string.format("warn(%q); return 1", large_text),
})
assert(large_response.ok == true)
local large_warn
for index = before_large_warn + 1, #sent do
    local candidate = json_decode(sent[index])
    if candidate.type == "event" and candidate.event == "warn" then
        large_warn = candidate
        break
    end
end
assert(large_warn, "large warn inside run was not forwarded")
assert(#large_warn.text <= 4096 + #"... [truncated]", "warn text was not truncated")
assert(
    large_warn.traceback and #large_warn.traceback <= 4096 + #"... [truncated]",
    "traceback was not truncated"
)
assert(warnings[#warnings] == large_text, "original warn did not receive the full text")

local dbg_response, before_dbg = request("run-dbg", "run", {
    code = "dbg('debug', { value = 3 }); return 1",
})
assert(dbg_response.ok == true)
local dbg_event
for index = before_dbg + 1, #sent do
    local candidate = json_decode(sent[index])
    if candidate.type == "event" and candidate.event == "dbg" then
        dbg_event = candidate
        break
    end
end
assert(dbg_event, "dbg inside run was not forwarded")
assert(dbg_event.text:find("debug", 1, true))
assert(dbg_event.text:find("value = 3", 1, true))

local isolation = request("run-isolation", "run", {
    code = "scoped_value = 1; _G.scoped_g = 2; return scoped_value, _G.scoped_g",
})
assert(isolation.ok == true)
assert(isolation.result.results[1] == "1")
assert(isolation.result.results[2] == "2")
assert(runtime_genv.scoped_value == nil, "run leaked a global into getgenv")
assert(_G.scoped_value == nil, "run leaked a global into _G")
assert(_G.scoped_g == nil, "run leaked a _G write into the real _G")

response = request("workspace", "workspace", { depth = 2, max_nodes = 20 })
assert(response.ok == true)
assert(response.result.root.name == "Workspace")
assert(response.result.root.full_name == "Workspace")
assert(response.result.nodes == 4, "workspace should contain root, two children, and one grandchild")
local child_names = {}
for _, child in ipairs(response.result.root.children) do
    child_names[child.name] = true
end
assert(child_names.TargetPart)
assert(child_names.Model)

response = request("search", "search", { name = "TargetPart", exact = true })
assert(response.ok == true)
assert(response.result.matched == 1, "expected exactly one TargetPart")
assert(response.result.matches[1].full_name == "Workspace.TargetPart")
assert(response.result.matches[1].parent.full_name == "Workspace")
assert(response.result.matches[1].parent_full_name == "Workspace")

response = request("search-substring", "search", { name = "Part" })
assert(response.ok == true)
assert(response.result.matched >= 3, "substring search should find all Part instances")

response = request("search-properties", "search", {
    properties = { ClassName = "TextButton", Text = "Play" },
})
assert(response.ok == true)
assert(response.result.matched == 1, "expected one TextButton with Text = Play")
assert(response.result.matches[1].full_name == "ScreenGui.PlayButton")
assert(response.result.matches[1].properties.ClassName == "TextButton")
assert(response.result.matches[1].properties.Text == "Play")
assert(response.result.property_filters == 2)

response = request("search-type-alias", "search", {
    properties = { Type = "TextButton" },
})
assert(response.ok == true)
assert(response.result.matched == 2, "Type should alias ClassName")

response = request("search-alternatives", "search", {
    properties = { Type = { "TextButton" }, Text = { "Play", "Close" } },
})
assert(response.ok == true)
assert(response.result.matched == 2, "array property values should match alternatives")

response = request("search-name-properties", "search", {
    name = "Play",
    properties = { ClassName = "TextLabel" },
})
assert(response.ok == true)
assert(response.result.matched == 1, "name and property filters should combine")
assert(response.result.matches[1].name == "PlayLabel")

response = request("search-property-miss", "search", {
    properties = { Text = "Nope" },
})
assert(response.ok == true)
assert(response.result.matched == 0)

response = request("search-no-filter", "search", {})
assert(response.ok == false)
assert(response.error:find("name and/or at least one property", 1, true))

response = request("search-empty-properties", "search", { properties = {} })
assert(response.ok == false)
assert(response.error:find("name and/or at least one property", 1, true))

response = request("join", "join", { place_id = 1818 })
assert(response.ok == true)
assert(response.result.place_id == 1818)
assert(response.result.scheduled == true)
assert(teleport_calls[1] == 1818, "join did not call TeleportService:Teleport")

response = request("join-string", "join", { place_id = "42" })
assert(response.ok == true)
assert(response.result.place_id == 42)
assert(teleport_calls[2] == 42)

response = request("join-invalid", "join", { place_id = 0 })
assert(response.ok == false)
assert(response.error:find("positive integer", 1, true))

next_teleport_error = "teleport blocked"
local before_join_error = #sent
socket.OnMessage.callbacks[1](json_encode({
    id = "join-error",
    method = "join",
    params = { place_id = 99 },
}))
local join_response
local join_error_event
for index = before_join_error + 1, #sent do
    local candidate = json_decode(sent[index])
    if candidate.type == "response" and candidate.id == "join-error" then
        join_response = candidate
    elseif candidate.type == "event" and candidate.event == "protocol_error" then
        join_error_event = candidate
    end
end
assert(join_response and join_response.ok == true, "join response is missing")
assert(join_error_event, "teleport error event is missing")
assert(join_error_event.text:find("teleport blocked", 1, true))

response = request("unknown", "nope", {})
assert(response.ok == false)
assert(response.error:find("unknown method", 1, true))

io.write("runtime_spec: ok\n")
