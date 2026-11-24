<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/rome-logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/rome-logo-light.svg">
    <img alt="ROME" src="assets/rome-logo-light.svg" width="420">
  </picture>
</p>

# ROME (Rockstar Online Modding Engine)

Unofficial concept. Strictly experimental. Not affiliated with Rockstar Games, Take-Two Interactive or Cfx.re.

ROME is a multiplayer and modding framework for the GTA VI PC port. It follows the CitizenFX model used by FiveM and RedM: a dedicated server hosts resources, resources are streamed to clients, and scripts in Lua or JavaScript drive the game through natives, events and replicated state.

This README is the design spec. Nothing below is implemented yet.

## Status

There is no playable GTA VI PC port yet. Work on the server, scripting runtimes, resource system and network protocol comes first, since none of it needs the game. The client attaches to a port through an adapter layer, so ROME is not tied to one port.

## Components

| Component | Description |
|---|---|
| `rome-server` | Headless dedicated server for Windows and Linux. Hosts resources, owns game state and runs server scripts. |
| `rome-client` | Injected into the game process. Runs client scripts, applies synced state and renders UI. |
| `rome-launcher` | Checks the local game install, loads the client and handles server connect and disconnect. |
| Adapters | One per supported PC port. Map ROME's game interface (tick, entities, natives, input, rendering) onto that port. |

Core is C++20, built with CMake, with dependencies managed through vcpkg.

## Server

The server reads `server.cfg` on startup. It uses a FiveM-style command syntax.

```cfg
endpoint_add_tcp "0.0.0.0:30200"
endpoint_add_udp "0.0.0.0:30200"

sv_hostname "ROME dev server"
sv_maxclients 64

set rome_syncRate 30            # entity sync rate (Hz)
set rome_cullingRadius 424.0    # entity scope distance (m)
set rome_entityLockdown strict  # inactive | relaxed | strict

add_ace group.admin command allow
add_principal identifier.rome:1 group.admin

ensure spawnmanager
ensure freeroam
```

The server opens TCP and UDP on the same port:

- **TCP (HTTP):** the connect handshake, resource file downloads and the public endpoints `/info.json` and `/players.json`.
- **UDP:** game traffic, using ENet.

Console commands: `start`, `stop`, `restart`, `ensure`, `refresh`, `status`, `kick`, `quit`.

## Resources

A resource is a folder with a `rome.toml` manifest. The server loads resources from `resources/`. A folder name in brackets (such as `resources/[core]/`) only groups resources and is not a resource itself.

```toml
[resource]
name        = "freeroam"
version     = "0.1.0"
game        = "gta6"
description = "Basic freeroam gamemode"

[scripts]
shared = ["shared/config.lua"]
client = ["client/*.lua", "client/*.js"]
server = ["server/*.lua"]

[ui]
page  = "ui/index.html"
files = ["ui/**"]

[files]
client = ["data/*.json"]

[dependencies]
resources = ["spawnmanager"]
```

- **Runtimes:** chosen by file extension. `.lua` runs in Lua and `.js` runs in JavaScript.
- **Globs:** supported in every file list.
- **Load order:** shared scripts load first, then client or server scripts, in the order listed.
- **Dependencies:** start before the resource that needs them. If one is missing, the resource does not start.
- **Integrity:** client files are hashed and cached. Clients only re-download files whose hash has changed.
- **Lifecycle events:** `onResourceStart`, `onResourceStop`, `onClientResourceStart` and `onClientResourceStop`.

## Scripting

### Runtimes

| Runtime | Client | Server |
|---|---|---|
| Lua | Lua 5.4 | Lua 5.4 |
| JavaScript | V8 | Node.js 22 LTS |

Each resource gets its own isolated environment per runtime. Resources do not share globals. They talk to each other through events, exports and state bags.

The Lua runtime adds native `vector2`, `vector3`, `vector4` and `quat` types with arithmetic operators. It also adds backtick literals that hash to joaat at compile time, for example `` `adder` ``.

### Threads

Scripts run on a cooperative scheduler. There is no preemption, so a loop without `Wait` freezes its runtime.

- **Client:** the scheduler runs once per game frame.
- **Server:** the scheduler runs once per server tick.

```lua
CreateThread(function()
    while true do
        Wait(0) -- yield until next tick
        -- per-frame logic
    end
end)
```

`SetTimeout(ms, fn)` schedules a one-shot callback.

### Events

| Function | Side | Description |
|---|---|---|
| `AddEventHandler(name, fn)` | both | Listen for an event |
| `RegisterNetEvent(name)` | both | Allow an event to be received from the network |
| `TriggerEvent(name, ...)` | both | Fire a local event |
| `TriggerServerEvent(name, ...)` | client | Send to the server |
| `TriggerClientEvent(name, target, ...)` | server | Send to one client, or `-1` for all |
| `CancelEvent()` | both | Cancel the current event, where cancellable |

- **Network registration:** a net event must be registered with `RegisterNetEvent` before it is handled. Unregistered net events are dropped.
- **Sender:** in a server handler, `source` is the sending player's server ID.
- **Payloads:** serialized with MessagePack. Supported types are nil, booleans, numbers, strings, tables or arrays, and vectors. Functions cannot be sent over the network.

### Exports

```lua
-- resource "bank"
exports('getBalance', function(id) return balances[id] or 0 end)

-- any other resource
local bal = exports.bank:getBalance(playerId)
```

Exports work across runtimes, so a Lua export can be called from JavaScript and the reverse.

### State bags

Key/value state that replicates attached to an entity, a player or the server.

```lua
-- server
Entity(vehicle).state:set('fuel', 62.5, true)  -- true = replicate to clients
Player(src).state:set('job', 'mechanic', false) -- server-only
GlobalState.weather = 'storm'

-- client
local fuel = Entity(vehicle).state.fuel
AddStateBagChangeHandler('fuel', nil, function(bagName, key, value) end)
```

Only the server can write state that replicates. Clients can write local state only, unless a resource explicitly allows client writes on a key.

### Natives

Game functions are exposed as natives, each identified by a 64-bit hash. ROME keeps its own native database in `natives/`. Code generation from that database produces:

- Lua and JS bindings
- TypeScript and LuaLS type definitions
- documentation

Each adapter maps the native hashes onto its port.

```lua
local ped = PlayerPedId()
local coords = GetEntityCoords(ped)
SetEntityHeading(ped, 90.0)

-- raw call by hash (placeholder hash)
InvokeNative(0x0000000000000000, ped, 90.0)
```

Server natives are a separate set. They act on synced state, such as creating networked entities, setting routing buckets and reading player identifiers.

### UI

The `ui.page` in the manifest is rendered with embedded Chromium (CEF) on top of the game.

| Call | Purpose |
|---|---|
| `SendUIMessage(data)` | Sends a message from the script to the page. The page receives it as `message` events. |
| `RegisterUICallback(name, fn)` | Handles a request from the page. The page sends it as a `fetch` to `https://<resource>/<name>`. |
| `SetUIFocus(hasFocus, hasCursor)` | Moves keyboard and mouse focus between the game and the page. |

### Example

```lua
-- server/main.lua
RegisterNetEvent('freeroam:spawnVehicle')

AddEventHandler('playerJoining', function()
    print(('%s joined (%s)'):format(GetPlayerName(source), source))
end)

AddEventHandler('freeroam:spawnVehicle', function(model)
    local src = source
    if type(model) ~= 'string' then return end

    local ped = GetPlayerPed(src)
    local pos = GetEntityCoords(ped)
    local veh = CreateVehicle(joaat(model), pos.x + 2.0, pos.y, pos.z, GetEntityHeading(ped), true, true)
    Entity(veh).state:set('owner', src, true)
end)
```

```js
// client/main.js
RegisterCommand('car', (source, args) => {
  emitNet('freeroam:spawnVehicle', args[0] ?? 'default_sedan');
}, false);
```

## Networking

- **Transport:** ENet over UDP. Channels are reliable-ordered (events, state bags), unreliable-sequenced (entity sync) and reliable-unordered (bulk data).
- **Authority:** the server decides what exists. Clients simulate the entities they own, and the server checks and relays their updates.
- **Ownership:** an entity is owned by the client best placed to simulate it, usually the closest player. Ownership migrates as players move. Scripts can pin ownership to a specific player or to the server.
- **Scope:** a client only receives entities within `rome_cullingRadius` of its player, plus entities a script has marked as persistent.
- **Routing buckets:** players and entities can be split into separate buckets on one server. Each bucket is its own world and does not see the others.
- **Population:** ambient peds and traffic are on by default and can be turned off per bucket.

## Security

- **Untrusted clients:** the server never trusts the client. Every net event handler must validate its arguments.
- **Entity creation:** `rome_entityLockdown` controls whether clients may create networked entities. `inactive` allows all, `relaxed` blocks client-created entities that no script owns, and `strict` allows server-created entities only.
- **Permissions:** ACE permissions (`add_ace`, `add_principal`, `IsPlayerAceAllowed`) gate commands and resource access.
- **Rate limits:** applied per client to net events and state bag writes.

## Differences from FiveM

- **Runtimes:** Lua and JavaScript only. There is no C# runtime.
- **Manifest:** TOML (`rome.toml`) instead of `fxmanifest.lua`.
- **Game access:** goes through adapters, because there is no single official PC executable to target.
- **APIs:** event, export and state bag APIs mirror FiveM's on purpose, so porting resources is mostly mechanical. Natives will differ, because GTA VI's functions are not GTA V's.

## Legal

ROME doesn't ship game files, executables or copyrighted assets, and doesn't link to leaked or pirated material. Own the game.
