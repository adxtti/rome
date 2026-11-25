<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/rome-logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/rome-logo-light.svg">
    <img alt="ROME" src="assets/rome-logo-light.svg" width="420">
  </picture>
</p>

# ROME (Rockstar Online Modding Engine)

Unofficial concept. Strictly experimental. Not affiliated with Rockstar Games, Take-Two Interactive or Cfx.re.

ROME is a multiplayer and modding platform for the GTA VI PC port. The base model is the same as CitizenFX: a dedicated server hosts resources, clients stream them, and scripts drive the game through natives, events and replicated state. Most of what sits on top of that base is redesigned.

Components are namespaced `u_rome::` (unofficial ROME).

## Status

There is no playable GTA VI PC port yet. Work on the server, runtimes, tooling and protocol comes first, since none of it needs a running game. The client attaches to a port through an adapter, so ROME is not tied to one port.

## Components

| Component | Description |
|---|---|
| `u_rome::server` | Dedicated server for Windows and Linux. World state, resources, persistence, admin panel. |
| `u_rome::client` | Loaded into the port's process through an adapter. Client scripts, sync, UI. |
| `u_rome::launcher` | Checks the local install, loads the client, connects to servers. |
| `u_rome::cli` | The `rome` command: scaffolding, dev server with hot reload, build, test, package management. |
| `u_rome::studio` | Desktop tools: asset viewer, world viewer, map editor, native explorer, profiler. |
| `u_rome::formats` | Archive and asset decoding shared by Studio, the CLI and the server. |
| `u_rome::natives` | Native database, per-build hash maps and binding generation. |
| `u_rome::adapter::<port>` | One per supported PC port. Plugs into the port's import routing and talks to ROME over a C ABI. See [Adapters](#adapters). |

Written in Rust. The runtimes embed Luau (via `mlua`), V8 (via `rusty_v8`) and Wasmtime. Studio uses `egui` and `wgpu`.

## GTA VI internals

This is what public reverse-engineering projects have found so far, and what ROME assumes from it. None of it is confirmed against official documentation.

### Scripts

- **Script VM:** game logic runs as compiled script programs, the same model as GTA V and RDR2. [PercyStar](https://github.com/rpatty-dev/PercyStar) finds `scrProgram` anchors in the executable. `scrProgram` is the RAGE class for a compiled script.
- **Decompiled listings:** [openrage](https://github.com/openragee/openrage/pull/10) lifts decompiled GTA VI script listings to readable source. That is 746 listings and 572,370 functions. The listings show GTA V's layout: globals, statics, functions, and native calls identified by hash.
- **Bytecode:** compiled scripts can't be read directly yet, because the archive table that holds them is encrypted.
- **Natives:** no native table, native names or opcode table for GTA VI has been published.

### Data

- **`script.rpf`:** holds data, not bytecode. That data is 1,012 PSIN documents (PSO v122). The chunk chain is `PSIN, LSCH, LENM, LSNS, LGID, LRSC, CHKS` ([rage9-re](https://github.com/Ahmet-Dedeler/rage9-re/blob/master/docs/SCRIPT_FORMAT.md)).
- **Hashing:** names in data files are 32-bit JOAAT hashes, and a 64-bit hash pool also exists. Projects disagree on whether names are lowercased before hashing.
- **Name registry:** [Rage Tools VI](https://github.com/lynxnathan/rage-tools-vi) keeps a registry of about 37,000 hash-to-name pairs.

### Parser metadata

- **What it is:** RAGE registers every data structure with its parser (`parManager`), including fields, offsets, types and defaults.
- **Prior work:** [rage-parser-dumps](https://github.com/alexguirre/rage-parser-dumps) pulls this out of GTA IV, GTA V and RDR at runtime by injecting a DLL and pattern-scanning for `parManager`.
- **GTA VI:** there is no GTA VI dump yet. Making one needs a running build and new scan patterns.

### What ROME assumes

- **Native calls:** natives are called the way FiveM calls them in GTA V. The adapter looks up the handler by hash in the game's native registration table and calls it with an argument array and a return slot. Scripts see `natives.invoke(hash, ...)`.
- **Native database:** `u_rome::natives` is built up from decompiled listings. Every call site gives a hash, plus argument and return counts if the format matches GTA V's. The community fills in names and signatures, as it did for GTA V's NativeDB.
- **Changing hashes:** if hashes change between game builds, the adapter applies a per-build crossmap, as FiveM does for GTA V.
- **Parser schemas:** once a port runs, a dumper for GTA VI exports parser schemas in the rage-parser-dumps JSON format. From those, `rome build` generates typed game data for Luau and TS, such as vehicle handling and weapon stats. Studio also uses them to show data files with real field names instead of hashes.

## Adapters

### How the ports run the game

The leading ports, [XCL](https://github.com/xcl-dev/xcl) and [VIctory](https://github.com/Marvisak/VIctory), do not emulate the CPU:

- **Loading:** a PE loader maps the Xbox executable, and its x64 code runs natively.
- **Console APIs:** reimplemented in place. The port's import routing sends each import to one of three handlers: its own HLE module, the host OS, or a stub.
- **GPU:** shaders are the only code that gets recompiled, from RDNA2 to HLSL to DXIL.
- **Files:** console drive paths (`GAME:`, `S:`, `T:`) are redirected to folders on the PC. XCL writes changes to an overlay and never touches the game folder.
- **Network:** VIctory is offline by design. XCL forwards networking to Xbox services that no longer answer.

Neither port has a plugin or mod API yet. VIctory keeps game-specific hooks in one crate, `victory-game`. Everything else in it is generic.

### What an adapter does

An adapter is a module registered in the port's import routing. It is not a DLL injected the way FiveM does it. It provides ROME's game interface:

| Interface | Hook point |
|---|---|
| Tick | The game's main loop. Drives the client scheduler once per frame. |
| Natives | Handler lookup by hash in the native registration table. |
| Entities | Entity pools and the game's network object layer, for sync. |
| Files | The port's drive redirection. Streamed resources are mounted as an overlay. |
| Input | The port's GameInput layer. |
| Rendering | Frame present (PresentX). UI and overlays draw here. |
| Network | None from the port. ROME brings its own QUIC transport. |

Both ports are GPL (XCL is GPL-2.0-only, VIctory is GPL-3.0-or-later), so each adapter is its own GPL crate. Adapters talk to ROME through a stable C ABI. That keeps the core independent of any one port's license.

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
set rome_gameData ""             # optional path to your own game install, enables world validation

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

**World data:**

With `rome_gameData` set, the server loads collision and navmesh data from the operator's own install. FiveM servers have no world geometry. ROME servers can use it to:

- check player movement against real ground and walls
- raycast line of sight on hit claims
- pathfind server-controlled NPCs on the navmesh

Collision is stored as quantised BVH trees with named surface materials. Navmesh comes in 128 m tiles, with road surfaces flagged. Both are documented in [GTA6-Asset-Info](https://github.com/TrinitronClient/GTA6-Asset-Info).

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

The UI is built with `egui` and `egui_dock`, and renders through `wgpu`.

The World Viewer has to handle a lot of data:
- 2.25M placed entities across about 30,800 map files
- 24.9M prop instances
- 219M grass instances

It streams this content in LOD rings around the camera, batched by the game's 60 m, 512 m and 1024 m sector grids. Props and grass are drawn with GPU instancing. The game's shaders exist only as RDNA2 machine code, so Studio has its own material renderer, built on the 344 named game shaders and their parameters.

### Plugins

Studio plugins are WASM modules with capability-based access. The design follows [openrage](https://github.com/openragee/openrage):

- **Capabilities:** a plugin declares what it needs in `plugin.json`, such as `read_archive`, `edit_document`, `add_panel` or `draw_overlay`.
- **Enforcement:** an import the plugin did not declare fails at load time.
- **Consent:** the user approves a plugin before its first run, and again whenever its manifest changes.
- **Limits:** execution is fuel-limited, and shipped game files are never written.

### Formats

`u_rome::formats` handles archive and asset decoding.

**How archives are read.** The RPF9 table of contents is encrypted, and no public key exists. Like every current tool, ROME reads archives in two steps:

1. **Scan.** Find zlib streams at 16-byte steps, verify them with Adler-32, and join 64 KiB chunks into whole files.
2. **Index.** Store a persistent, content-addressed index of everything found. Archives are memory-mapped.

File names are recovered from paths found inside the data and from known JOAAT name lists. The first index build is a one-time cost.

| Format | Approach | Source | License |
|---|---|---|---|
| RPF9 archives | Scan and index, clean-room | [rage-tools-vi](https://github.com/lynxnathan/rage-tools-vi), [0x52504639](https://github.com/SentinelInteractive/0x52504639) notes | MIT |
| TTPF / RDS5 textures | Chunk table (raw, zlib, BCPack), AMD Gfx10 detiling | [ttpf-studio](https://github.com/woreksami/ttpf-studio) notes | MIT |
| BCPack | Library. Only BC1 is reliable, and BC3 to BC7 are unfinished, so output is provisional. Raw or zlib mips are preferred when present. | [bcpack-rs](https://github.com/Ahmet-Dedeler/bcpack-rs) | MIT / Apache-2.0 |
| Models | Clean-room: quantised positions, meshlet indices, LOD groups, skeletons | [GTA6-Asset-Info](https://github.com/TrinitronClient/GTA6-Asset-Info) docs | Docs only |
| Map placements, collision, navmesh | Clean-room | [GTA6-Asset-Info](https://github.com/TrinitronClient/GTA6-Asset-Info) docs | Docs only |
| PSO / PSIN data, animations | Library | [rage9-re](https://github.com/Ahmet-Dedeler/rage9-re) | MIT / Apache-2.0 |
| AWC audio | Clean-room. Opus packets are wrapped in Ogg without re-encoding. | [AWC extractor](https://github.com/Albotterin/AWC_from_RPF9_Extractor) notes | MIT |
| Bink 2 video | Not supported. No open decoder exists. | | |
| Parser schemas | [rage-parser-dumps](https://github.com/alexguirre/rage-parser-dumps) JSON format. No GTA VI dump yet. | rage-parser-dumps | MIT |
| Hash names | Registry | [rage-tools-vi](https://github.com/lynxnathan/rage-tools-vi) | MIT |

Code is only taken from MIT or Apache projects. Some projects are used for reference only:
- **Copyleft:** SixNG, VIctory, XCL and openrage's `or-media`.
- **All rights reserved:** PercyStar.
- **No license:** 6stars and rpf9-explorer.

The [awesome-gta6-reverse-engineering](https://github.com/Ahmet-Dedeler/awesome-gta6-reverse-engineering) list tracks the wider tooling scene.

## Building

Requires Rust 1.100 (pinned in `rust-toolchain.toml`).

```sh
cargo run -p u_rome_studio --release   # ROME Studio
cargo run -p u_rome_cli -- --help      # rome CLI
```

Windows x64 and ARM64 builds of Studio and the CLI are attached to every CI run on `staging` and `main`.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Port adapters will be licensed separately to match the port they target.

### Credits

- BCPack texture decoding and Gfx10 detiling build on [bcpack-rs](https://github.com/Ahmet-Dedeler/bcpack-rs) by Ahmet Dedeler (MIT OR Apache-2.0).
- Opus decoding uses [opus-decoder](https://crates.io/crates/opus-decoder) (MIT OR Apache-2.0).

## Legal

ROME doesn't ship game files, executables or copyrighted assets, and doesn't link to leaked or pirated material. Own the game.
