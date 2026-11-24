# ROME — Rockstar Online Modding Engine (Unofficial)

> **Unofficial concept. Strictly experimental.**
> ROME is not affiliated with, endorsed by, or connected to Rockstar Games, Take-Two Interactive, or Cfx.re. "Rockstar" and "Grand Theft Auto" are trademarks of their respective owners.

ROME is a community multiplayer and modding framework for the Grand Theft Auto VI PC port effort, in the spirit of FiveM/RedM. It aims to provide dedicated servers, client synchronization, and a multi-language scripting environment on top of whichever community PC port becomes viable.

---

## Table of contents

- [Ecosystem status](#ecosystem-status)
- [Design principles](#design-principles)
- [Architecture](#architecture)
- [Scripting](#scripting)
- [Resources & manifests](#resources--manifests)
- [Networking](#networking)
- [Roadmap](#roadmap)
- [Repository layout](#repository-layout)
- [Branches](#branches)
- [Legal](#legal)

---

## Ecosystem status

*Snapshot as of 2026-11-24. Changes daily.*

GTA VI launched on PlayStation 5 and Xbox Series X|S. There is **no official PC release**. Community efforts are trying to run the Xbox Series build (v1.0.2.1) on PC using several approaches:

| Approach | Examples | Notes |
|---|---|---|
| Compatibility layer | XCL, VIctory (Rust) | Translates Xbox system/GPU calls at runtime |
| Static recompilation | Project ReCity, Leonida (taken down) | Ahead-of-time translation to native x64 |
| HLE emulator | various prototypes | High-level emulation of the console |
| Linux / Proton | TuxBox | Linux-first targets |

**No effort is playable yet.** According to [PortWatch VI](https://portwatch-vi.xyz/), the leading builds have reached "in the world" stages at single-digit frame rates, with stability problems such as GPU watchdog resets.

**What this means for ROME:** ROME cannot hook a stable target yet. Work starts on the parts that do not depend on the game, such as the server, the scripting runtimes, the resource system, and the network protocol. The game-facing layer is built behind an adapter interface, so it can attach to whichever port matures first.

> ⚠️ Fake "GTA VI PC" downloads and installers carrying malware are circulating. ROME will never ship game binaries.

---

## Design principles

1. **Port-agnostic.** All game interaction goes through a *Host Adapter*. ROME should not care whether the game is running under a compat layer, a recomp, or an emulator.
2. **Server-authoritative.** The server owns game state. Clients are treated as untrusted.
3. **Pluggable runtimes.** Scripting languages are components and can be added or removed without changing the core.
4. **No game assets in the repo.** Nothing copyrighted is committed. Formats are read at runtime from the user's own files.
5. **Clean-room.** ROME does not copy code from Cfx.re/FiveM or any Rockstar source. It uses public docs only for conceptual inspiration.

---

## Architecture

```
┌──────────────────────────── CLIENT ────────────────────────────┐
│                                                                 │
│   ┌──────────────┐   ┌───────────────┐   ┌──────────────────┐   │
│   │  rome-core   │──▶│ Script Host   │──▶│ Runtimes          │   │
│   │ (components, │   │ (resources,   │   │ Lua · JS · C#     │   │
│   │  object model│   │  events, ABI) │   └──────────────────┘   │
│   │  lifecycle)  │   └───────┬───────┘                          │
│   └──────┬───────┘           │ native calls                     │
│          │           ┌───────▼────────┐                         │
│          │           │ Native Bridge  │  hash → handler table   │
│          │           └───────┬────────┘                         │
│          │           ┌───────▼────────┐                         │
│          │           │ Host Adapter   │  XCL / ReCity / …       │
│          │           └───────┬────────┘                         │
│          │                   ▼                                  │
│          │           [ GTA VI PC port ]                         │
│   ┌──────▼───────┐                                              │
│   │  rome-net    │◀──────────── UDP (reliable + unreliable) ───┐│
│   └──────────────┘                                             ││
└────────────────────────────────────────────────────────────────┘│
                                                                  │
┌──────────────────────────── SERVER ─────────────────────────────┤
│   ┌──────────────┐   ┌───────────────┐   ┌──────────────────┐   │
│   │ rome-server  │──▶│ Script Host   │──▶│ Runtimes          │   │
│   │ (sessions,   │   │ (server-side  │   │ Lua · JS · C#     │   │
│   │  auth, state)│   │  resources)   │   └──────────────────┘   │
│   └──────┬───────┘   └───────────────┘                          │
│   ┌──────▼───────┐                                              │
│   │ Sync Engine  │  entity ownership · scope · state bags  ◀────┘
│   └──────────────┘                                              │
└─────────────────────────────────────────────────────────────────┘
```

### Components

| Component | Responsibility |
|---|---|
| **rome-core** | Component loader, object model (interfaces + refcounting), lifecycle, logging, config |
| **rome-net** | Transport (UDP with reliable/ordered channels), packet framing, compression, encryption |
| **rome-server** | Headless dedicated server: player sessions, auth, resource distribution, console/RCON |
| **sync engine** | Entity replication, ownership migration, distance-based scoping, state bags |
| **script host** | Loads resources, routes events, exposes the native ABI to runtimes |
| **runtimes** | Lua, JavaScript (V8), and C# (.NET) as separate pluggable components |
| **native bridge** | Maps stable native hashes to handlers. Lets scripts call game functions through the adapter |
| **host adapter** | Hooks one specific PC port: entry points, game loop tick, memory/entity access, rendering overlay |
| **launcher** | Validates the user's game install, injects the client, and connects to servers |

---

## Scripting

ROME will support three runtimes. Each one is a separate component behind a common Script Host ABI.

| Runtime | Engine | Use case |
|---|---|---|
| **Lua** | Lua 5.4 (with integer/vector extensions) | Lightweight gameplay scripts, the default |
| **JavaScript** | V8 (client), Node.js (server) | npm ecosystem, web-style tooling |
| **C#** | .NET (modern CoreCLR, not Mono) | Large typed codebases, frameworks |

### Core scripting API (planned)

```lua
-- server.lua
AddEventHandler('playerJoining', function()
    local src = source
    print(('Player %s joined'):format(GetPlayerName(src)))
    TriggerClientEvent('rome:welcome', src, 'Welcome to Leonida')
end)

RegisterCommand('car', function(src, args)
    local model = args[1] or 'default_sedan'
    local ped = GetPlayerPed(src)
    local x, y, z = table.unpack(GetEntityCoords(ped))
    CreateVehicle(joaat(model), x + 2.0, y, z, 0.0, true)
end)
```

- **Events:** local, client→server, and server→client events, with payload validation.
- **Exports:** cross-resource function calls, which also work across runtimes.
- **State bags:** replicated key/value state on entities, players, and global scope.
- **Natives:** game functions identified by hash and exposed through the Native Bridge. The native database is generated, not hand-maintained.
- **Threads:** cooperative coroutines (`Wait(ms)`) that are tied to the game tick.

---

## Resources & manifests

A **resource** is a folder of scripts, assets, and UI that the server streams to clients. Each resource has a `rome.toml` manifest:

```toml
[resource]
name        = "freeroam"
version     = "0.1.0"
game        = "gta6"
rome_version = ">=0.1"

[scripts]
client = ["client/*.lua"]
server = ["server/*.lua", "server/*.js"]
shared = ["shared/config.lua"]

[ui]
page  = "ui/index.html"
files = ["ui/**"]

[dependencies]
resources = ["spawnmanager"]
```

- Runtimes are picked by file extension: `.lua`, `.js`, and `.dll` (.NET).
- Asset streaming for RPF9-format content is planned once the format is documented by the community.

---

## Networking

- **Transport:** UDP with separate reliable-ordered, reliable-unordered, and unreliable channels.
- **Authority:** server-authoritative state. Clients propose and the server validates.
- **Entity sync:** ownership is assigned to the nearest client and migrates as players move. Scoping is distance-based, so each client only receives the entities it can see.
- **Scale target:** 128 players for v1, with the architecture designed for 1000+.
- **Security:** encrypted sessions, rate limiting, event allowlists, and server-side validation of entity creation.

---

## Roadmap

ROME milestones are gated on the state of the community ports.

| Phase | Depends on port | Deliverable |
|---|---|---|
| **0. Foundations** | No | Repo, core object model, build system, CI |
| **1. Server** | No | Headless server, Lua/JS/C# runtimes, resource loader, events, console |
| **2. Protocol** | No | Network transport, mock client, sync engine against simulated entities |
| **3. Adapter SDK** | Port reaches *exe runs* | Host Adapter interface, first adapter against the leading port |
| **4. Client** | Port reaches *in the world* | In-game script host, native bridge, overlay UI |
| **5. Multiplayer** | Port reaches *playable* | Player sync, vehicles, peds, and a freeroam reference resource |
| **6. Ecosystem** | Port is stable | Server browser, launcher, docs, and a resource registry |

---

## Repository layout

Planned:

```
rome/
├── core/          # rome-core: components, object model
├── net/           # rome-net: transport + protocol
├── server/        # rome-server
├── sync/          # sync engine
├── scripting/
│   ├── host/      # script host + ABI
│   ├── lua/
│   ├── js/
│   └── dotnet/
├── natives/       # native DB + codegen
├── adapters/      # one folder per PC port
├── client/        # in-game client
├── launcher/
├── resources/     # reference resources (freeroam, spawnmanager…)
└── docs/
```

---

## Branches

| Branch | Purpose |
|---|---|
| `main` | Default branch for active development |
| `staging` | Integration and testing before release |
| `prod` | Stable releases only |

---

## Legal

- ROME **does not distribute** game files, executables, keys, or copyrighted assets, and never will.
- ROME does not link to or support leaked or pirated material.
- You must own a legitimate copy of the game.
- This is a non-commercial research project. If a rights holder has concerns, open an issue.
