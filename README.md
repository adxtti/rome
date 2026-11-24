<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/rome-logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/rome-logo-light.svg">
    <img alt="ROME" src="assets/rome-logo-light.svg" width="420">
  </picture>
</p>

# ROME (Rockstar Online Modding Engine)

Unofficial concept. Strictly experimental. Not affiliated with Rockstar Games, Take-Two Interactive or Cfx.re.

ROME is a multiplayer and modding platform for the GTA VI PC port. The base model is the same as CitizenFX: a dedicated server hosts resources, clients stream them, and scripts drive the game through natives, events and replicated state. Most of what sits on top of that base is redesigned to fix what FiveM gets wrong.

This README is the design spec. Nothing below is implemented yet.

## Status

There is no playable GTA VI PC port yet. Work on the server, runtimes, tooling and protocol comes first, since none of it needs a running game. The client attaches to a port through an adapter, so ROME is not tied to one port.

## What FiveM gets wrong

| Problem in FiveM | ROME |
|---|---|
| Every resource shares one script thread per side. One bad loop drops the server tick for everyone. | Server resources run in isolated VMs on a worker pool. Each resource has a CPU budget per tick. Going over the budget throttles the resource, then suspends it. |
| Net events are untyped strings. Any client can fire `giveMoney` with whatever arguments it wants. | Every network call is declared in a typed interface. Arguments are validated, rate limited and permission checked before a handler runs. |
| No return values over the network, so every framework ships its own callback library. | RPC is built in and returns a value: `await api.spawnVehicle(model)`. |
| Lua is untyped, and JS has types only if you bring your own tooling. | Luau with strict typing, plus TypeScript compiled by the server. Types for natives, events and exports are generated. |
| Database access needs a third-party resource (oxmysql). | Persistence is built in: embedded SQLite by default, Postgres optional, with per-resource migrations. |
| Administration needs txAdmin as a separate wrapper. | A web admin panel and API are built in. |
| resmon shows script CPU time only. Lua has no debugger. | A built-in profiler covers CPU, memory, bandwidth, entities and UI cost per resource. Luau has a DAP debugger, and TypeScript uses Chrome DevTools. |
| Restarting a resource wipes its state. | Hot reload keeps persisted state and state bags. |
| Asset streaming re-downloads whole files, and the cache breaks. | Assets are stored as content-addressed chunks. Clients download deltas only, and a CDN can serve them. |
| No package manager. Frameworks fork and copy each other. | `rome add` with semver and a lockfile. |
| Scripts can't be tested without a running game. | `rome test` runs resources against a headless mock world. |
| Every NUI frame runs its own page, and HUDs push data every frame. | One shared CEF compositor. UI messages are batched per frame, and a native HUD layer handles high-frequency overlays. |

## Components

| Component | Description |
|---|---|
| `rome-server` | Dedicated server for Windows and Linux. World state, resources, persistence, admin panel. |
| `rome-client` | Runs inside the game process. Client scripts, sync, UI. |
| `rome-launcher` | Checks the local install, loads the client, connects to servers. |
| `rome` CLI | Scaffolding, dev server with hot reload, build, test, package management. |
| ROME Studio | Desktop tools: asset viewer, world viewer, map editor, native explorer, profiler. |
| Adapters | One per supported PC port. They map ROME's game interface onto that port. |

Written in Rust. The runtimes embed Luau (via `mlua`), V8 (via `rusty_v8`) and Wasmtime. Studio uses `wgpu`.

## Scripting

### Runtimes

| Runtime | Use |
|---|---|
| **Luau** | Default for gameplay scripts. Gradual typing, a fast interpreter, native codegen and a built-in sandbox. |
| **TypeScript** | Runs on V8 on both sides. The server compiles TS when it starts the resource. npm dependencies are bundled at build time. |
| **WASM** | Runs on Wasmtime. For heavy compute or any language that compiles to WASM (Rust, Zig, C, Go). |

Every runtime sees the same API. Bindings come from one interface definition, so a Luau resource, a TS resource and a Rust/WASM resource can call each other with full types.

Luau is based on Lua 5.1 with its own extensions, not Lua 5.4. Most FiveM Lua ports over with small changes. The biggest one is that Luau has no `goto`.

### Interface definitions

Each resource declares the calls it exposes in `api.ridl`. `rome build` turns this into:

- Luau types
- TypeScript types
- WASM bindings
- runtime validators

```
namespace freeroam

rpc spawnVehicle(model: string<1..32>) -> NetId
    from client
    rate 2/s
    ace "freeroam.spawn"

event vehicleSpawned(owner: PlayerId, vehicle: NetId)
    to clients

export getSpawnCount(player: PlayerId) -> u32
```

- **`rpc`:** a request that returns a value.
- **`event`:** fire-and-forget.
- **`export`:** a call from another resource on the same side.

Calls from a client are rejected unless they are declared `from client`. A call that fails validation never reaches its handler, and the failure is logged against the player.

### Example

```luau
--!strict
-- server/main.luau
local api = require("@self/api")
local Vehicle = require("@rome/vehicle")

local spawned: { [PlayerId]: number } = {}

api.spawnVehicle:handle(function(player, model)
    local ped = player.ped
    local veh = Vehicle.create(model, ped.position + vector.create(2, 0, 0), ped.heading)
    veh.state.owner = player.id
    spawned[player.id] = (spawned[player.id] or 0) + 1
    api.vehicleSpawned:broadcast(player.id, veh.netId)
    return veh.netId
end)

api.getSpawnCount:handle(function(player)
    return spawned[player] or 0
end)
```

```ts
// client/main.ts
import { spawnVehicle } from "@self/api";
import { commands, notify } from "rome";

commands.register("car", async ([model = "default_sedan"]) => {
  const netId = await spawnVehicle(model);
  notify(`Spawned vehicle ${netId}`);
});
```

### Execution model

- **Server isolation:** each server resource runs in its own VM.
- **World access:** scripts read a snapshot of the world from the last tick. Writes go into a queue and are applied together at the tick boundary. This is how resources run in parallel without locks, and it makes each tick deterministic.
- **Client natives:** these must run on the game thread, so client scripts run there under a per-frame budget. Pure compute can move to a worker with `task.worker`, which has no native access.
- **Budgets:** enforced through each runtime's interrupt mechanism (Luau interrupts, V8 `TerminateExecution`, Wasmtime epochs). Defaults are 2 ms per tick on the server and 1 ms per frame on the client, configurable per resource.
- **Scheduling:** there are no `Wait(0)` polling loops. Code subscribes to `onTick`, entity events, state changes or timers instead.

### Game API

Generated object APIs (`Ped`, `Vehicle`, `Player`, `World`) wrap the natives. Raw natives are still available through `natives.<Name>(...)` and `natives.invoke(hash, ...)`. The native database in `natives/` is the single source for:

- bindings
- types
- Studio's native explorer

Natives are mapped per adapter, because GTA VI's functions are not GTA V's.

### State

- **State bags:** typed state that replicates, on entities, players and the server. Only the server writes state that replicates.
- **`rome.persist`:** per-resource durable key/value storage that survives restarts and hot reloads.
- **`rome.db`:** async SQL. SQLite is embedded by default, and Postgres can be set with `rome_db`. Migrations live in `migrations/`.

## Resources

```toml
[resource]
name    = "freeroam"
version = "0.1.0"
game    = "gta6"

[scripts]
shared = ["shared/**/*.luau"]
client = ["client/main.ts"]
server = ["server/main.luau", "server/physics.wasm"]

[api]
file = "api.ridl"

[budget]
server_ms = 2.0
client_ms = 1.0

[ui]
page = "ui/index.html"

[dependencies]
spawnmanager = "^1.0"
```

| Command | Description |
|---|---|
| `rome new <name>` | Scaffold a resource |
| `rome dev` | Local server with hot reload |
| `rome build` | Compile TS, generate bindings from `api.ridl`, bundle |
| `rome test` | Run tests against a headless mock world |
| `rome add <pkg>` | Add a dependency (`rome.lock`) |
| `rome publish` | Publish to a package registry |

## Server

```cfg
endpoint "0.0.0.0:30200"        # QUIC (game + assets) and HTTP (info, admin)
sv_hostname "ROME dev server"
sv_maxclients 128

set rome_db "sqlite://data/rome.db"
set rome_cdnUrl ""               # optional external asset host
set rome_entityLockdown strict   # inactive | relaxed | strict

add_ace group.admin command allow
add_principal identifier.rome:1 group.admin

ensure freeroam
```

**Built-in endpoints:**

| Endpoint | Purpose |
|---|---|
| `/info.json` | Server info, as in FiveM |
| `/players.json` | Player list, as in FiveM |
| `/admin` | Web admin panel |
| `/api` | Admin API |
| `/metrics` | Prometheus metrics |

**Networking:**
- **Transport:** QUIC, with encryption, multiplexed streams and unreliable datagrams for entity sync.
- **Entity sync:** server-authoritative. The nearest client simulates an entity, and ownership migrates as players move. Clients only receive entities within their culling radius.
- **Isolation:** routing buckets split players and entities into separate worlds on one server.
- **Validation:** server-side sanity checks on movement, damage and entity creation.

## ROME Studio

Desktop tools that read from your own game install. No game data is ever bundled or uploaded.

| Tool | Description |
|---|---|
| Asset Viewer | Browse RPF9 archives. Preview models, textures, audio and PSO data. Export models to glTF. |
| World Viewer | Fly the map with streaming LODs. Connect to a dev server to see live entities, players, zones and routing buckets on top. |
| Map Editor | Place props, build interiors and zones. Exports as a resource. |
| Native Explorer | Search the native database, read docs and call natives live against a connected client. |
| Profiler | Live per-resource CPU, memory, bandwidth and UI cost, with frame and tick timelines. |
| Net Inspector | Watch RPCs, events and state bag changes as they happen, with validation failures highlighted. |
| Console | REPL into any resource on the client or the server. |

### Formats

The `rome-formats` crate handles archive and asset decoding. It builds on the community's reverse-engineering work, and only uses code whose license is compatible:

| Format | Source | License |
|---|---|---|
| BCPack textures | [bcpack-rs](https://github.com/Ahmet-Dedeler/bcpack-rs) | MIT / Apache-2.0 |
| PSO / PSIN data, animations | [rage9-re](https://github.com/Ahmet-Dedeler/rage9-re) | MIT / Apache-2.0 |
| RPF9 archives | Clean-room, from public format notes | |
| AWC audio | Clean-room | |

GPL-3.0 projects (SixNG, VIctory) and all-rights-reserved projects (PercyStar) are references only and are not used as code dependencies. The [awesome-gta6-reverse-engineering](https://github.com/Ahmet-Dedeler/awesome-gta6-reverse-engineering) list tracks the wider tooling scene.

## Differences from FiveM

| | FiveM | ROME |
|---|---|---|
| Runtimes | Lua 5.4, JS, C# | Luau, TypeScript, WASM |
| Network API | Untyped string events | Typed RPC, events and exports from `api.ridl` |
| Threading | Shared scheduler per side | Isolated VMs, worker pool, per-resource budgets |
| Manifest | `fxmanifest.lua` | `rome.toml` |
| Database | Third-party | Built in |
| Admin | txAdmin | Built in |
| Packages | None | `rome add`, semver, lockfile |
| Transport | ENet + HTTP | QUIC + HTTP |
| Tooling | CodeWalker (community) | ROME Studio |

## Legal

ROME doesn't ship game files, executables or copyrighted assets, and doesn't link to leaked or pirated material. Own the game.
