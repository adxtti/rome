# ROME (Rockstar Online Modding Engine)

Unofficial concept. Strictly experimental. Not affiliated with Rockstar Games, Take-Two Interactive or Cfx.re.

ROME is a multiplayer and modding framework for the GTA VI PC port. If you've used FiveM, you already know how this works: dedicated servers, resources, Lua/JS/C# scripting, natives, events, state bags. Same idea, built for GTA VI.

## Status

Early. There's no playable PC port of GTA VI yet, so we're building the parts that don't need the game first: the server, scripting runtimes, resource system and networking. The client side will hook into whichever community port gets stable first.

## Scripting

Three runtimes, same as FiveM:

- Lua 5.4
- JavaScript (V8 on the client, Node on the server)
- C# (.NET)

```lua
AddEventHandler('playerJoining', function()
    print(('%s joined'):format(GetPlayerName(source)))
end)

RegisterCommand('car', function(src, args)
    local ped = GetPlayerPed(src)
    local x, y, z = table.unpack(GetEntityCoords(ped))
    CreateVehicle(joaat(args[1] or 'default_sedan'), x + 2.0, y, z, 0.0, true)
end)
```

## Resources

Resources are folders with a manifest, streamed from the server to clients.

```toml
[resource]
name = "freeroam"
version = "0.1.0"
game = "gta6"

[scripts]
client = ["client/*.lua"]
server = ["server/*.lua", "server/*.js"]
shared = ["shared/config.lua"]
```

## Roadmap

1. Core, build system, CI
2. Server with Lua/JS/C# runtimes and resource loading
3. Network protocol and entity sync
4. Client hooks into a PC port
5. Player, vehicle and ped sync, freeroam
6. Launcher, server browser, docs

## Branches

- `staging`: development
- `main`: production

## Legal

ROME doesn't ship game files, executables or copyrighted assets, and doesn't link to leaked or pirated material. Own the game.
