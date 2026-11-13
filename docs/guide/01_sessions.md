# 1. Sessions: host, join, offline

gdnet's first job is to run a session: a set of peers that agree on a protocol and share a tick. Everything
later (entities, replication, prediction, calls) runs inside one. This page covers starting and ending a
session, what gdnet does to your existing multiplayer code (nothing visible), and the network emulator.

## Starting a session

The `Net` singleton is available as soon as the gdnet extension is installed (`addons/gdnet/`). With no
call to it, nothing runs: no node, no polling, no packets.

```gdscript
# A server on UDP port 7777 (Godot's ENetMultiplayerPeer underneath).
Net.host(7777)

# A client. An optional third argument is its seat key (below).
Net.join("192.168.1.20", 7777)

# A single-player session: no network at all. This process hosts and plays as peer 1, so the same
# scripts run offline, as host and as client.
Net.offline()

# Any other MultiplayerPeer: WebRTC (browser), WebSocket, GodotSteam, Nakama...
var peer := WebRTCMultiplayerPeer.new()
# ... configure it as its own documentation says ...
Net.use_peer(peer)

# End the session (closes the peer, puts back the MultiplayerAPI that was there).
Net.leave()
```

`host`, `join`, `offline` and `use_peer` return an `Error` (`OK` on success). A peer whose
`get_unique_id()` is 1 hosts; any other joins.

### Seats, rejoining and saved sessions (WP17)

Every player has a *seat*: a number that stays the same for the session (1 is the host's player on a
listen server, else the first client), while its peer id changes with every connection. A client sends a
seat key when it joins: `Net.key` (a random string kept for the life of the process, or one the game sets,
such as a save slot or an account name) or `Net.join(address, port, key)`. A key whose seat dropped takes it
back; a second connection with the key of a connected seat is refused. Keys are identifiers, not
passwords: nothing checks them. `Net.dedicated = true` before `host`/`use_peer` makes a server that holds no
seat.

`Net.seat(peer)`, `Net.peer_of(seat)` (0 while the seat is away), `Net.my_seat()` and `Net.seat_table()`
answer on every peer; the other calls keep taking peer ids. A seated client that drops keeps its seat and
its entities for `Net.rejoin_timeout_s` seconds (`gdnet/session/rejoin_timeout_s`, default 60; 0 = gone at
once; -1 = for the whole session), and `peer_dropped(seat)`, `peer_rejoined(seat, peer)` and
`seat_released(seat)` tell the server and the other clients. `Net.save_state()` returns the session's keyed
state, seats and level states as bytes; `Net.host(port, state)`, `Net.offline(state)` and
`Net.use_peer(peer, state)` start from them.

### Joining is a handshake

When a client's transport connects, the client sends `Hello` (a hash of its protocol: the netcore version,
the tick rate, and in later versions the scene tables and profiles). The server compares it with its own and
answers `Welcome` (its current tick, the client's id) or refuses with a reason that names the first item
that differs:

```gdscript
Net.disconnected.connect(func(reason: String) -> void:
	print(reason))   # protocol mismatch: tick rate differs (server 60, client 30)
```

Both ends must therefore run the same physics tick rate (Project Settings > Physics > Common > Physics
Ticks Per Second).

### Signals and queries

| | |
|---|---|
| `peer_joined(id)` | a peer finished the handshake (on a client: the server, id 1) |
| `peer_left(id)` | a joined peer left |
| `peer_dropped(seat)`, `peer_rejoined(seat, peer)`, `seat_released(seat)` | a seat went away, came back under a new peer id, or was given up (WP17) |
| `connected` | this side is in the session: a client at `Welcome`, a host at its first tick |
| `disconnected(reason)` | the session ended on its own (refused, server lost); `leave()` emits nothing |
| `ticked(tick)` | once per physics tick while online |
| `Net.tick()` | the current tick (a client adopts the server's at `Welcome`) |
| `Net.peers()` | remote peers that finished the handshake (a client sees `[1]`) |
| `Net.rtt(id)` | smoothed round-trip time in milliseconds, `-1` before the first sample |
| `Net.is_online()` | from `host`/`join`/`offline`/`use_peer` until `leave()` or a disconnection |
| `Net.tick_rate` | physics ticks per second |

The tick is the physics tick: gdnet runs its loop in `_physics_process`, never on its own clock, so
`move_and_slide()` and your own `_physics_process` stay in step with it.

The per-tick signal is `ticked` (the method `tick()` returns the current tick):

```gdscript
Net.ticked.connect(func(t: int) -> void: print("tick ", t))
```

## Your existing multiplayer code keeps working

gdnet does not replace Godot's multiplayer; it wraps it. When a session starts, `Net` installs its own
`MultiplayerAPI` (`NetMultiplayerApi`, a `MultiplayerAPIExtension`) around the tree's `SceneMultiplayer`
with `SceneTree.set_multiplayer()`. Every call Godot makes (`rpc`, the configuration calls of
`MultiplayerSpawner` and `MultiplayerSynchronizer`, `poll`, the peer and id queries) is forwarded
unchanged, and the wrapped object's signals (`peer_connected`, `connected_to_server`, ...) are re-emitted.
So:

```gdscript
@rpc("any_peer")
func hello(text: String) -> void:
	print(multiplayer.get_remote_sender_id(), ": ", text)

# ... somewhere on a client
hello.rpc_id(1, "hi")      # works exactly as before
```

and a `MultiplayerSynchronizer` replicates as before. The test suite checks both (gate G1). gdnet's own
messages travel beside yours as raw bytes (`SceneMultiplayer.send_bytes()`) on channels 0 (reliable), 1 and
2 (unreliable), over whatever peer you chose, so a project can move to gdnet one scene at a time. `leave()`
restores the original `MultiplayerAPI` and its peer.

## Emulating a bad network

`NetEmulation` is a Resource with latency, variance, loss, reorder and duplication for what a side sends and
what it receives, plus Unreal's presets:

```gdscript
var emu := NetEmulation.new()
emu.preset("bad")              # 100-200 ms, 5 % loss, both directions ("average": 30-60 ms, 1 %)
emu.send_latency_ms = 150.0    # or set the numbers yourself; edits apply at once
Net.emulation = emu
```

The fields, each for both directions: `send_latency_ms` / `receive_latency_ms` (a fixed delay),
`send_variance_ms` / `receive_variance_ms` (an extra uniform random delay), `send_loss` / `receive_loss`,
`send_reorder` / `receive_reorder`, `send_duplicate` / `receive_duplicate` (probabilities 0..1), and `seed`
(the same seed gives the same schedule). `is_off()` is true while every value is zero. `preset(name)`
takes `off`, `lan`, `average`, `bad`, `mobile` or `custom` (the `gdnet/debug/emulation/<field>` settings);
`Net.emulation_preset(name)` does the same on `Net`'s emulation.

- Unreliable packets are dropped, delayed, reordered and duplicated as configured.
- Reliable packets behave like a reliable transport's: never lost or reordered, but each simulated loss
  costs a resend delay. At `loss = 1.0` the link is dead and nothing arrives, reliable or not.
- With `Net.offline()` and the in-memory `NetPipePeer`, emulation applies to every packet (yours and
  gdnet's) and delays are counted in physics ticks, so 100 ms at 60 Hz is 6 ticks, deterministically.
- With other peers (ENet, WebRTC, ...) gdnet can only delay its own messages; stock `@rpc` traffic is not
  emulated. For those, use the peer's own tools or test over `NetPipePeer`.

## Testing with several peers in one process

`NetPipePeer` connects ends in memory: `NetPipePeer.create_server()`, `NetPipePeer.create_client(server)`,
or `create_pair()`. Godot allows one `MultiplayerAPI` per subtree, so a single headless process can hold a
server branch and client branches, each with its own `NetSession` (the node `Net` uses internally).
`tests/framework/net_fixture.gd` builds that for tests:

```gdscript
extends TestCase

func test_clients_connect() -> void:
	var net := NetFixture.new(self, 2)       # /root/Server, /root/ClientA, /root/ClientB
	net.start()
	assert_true(await net.wait_connected())
	assert_eq(net.server_session.peers().size(), 2)
	net.teardown()
```

`NetFixture.run_both_modes(test, n, scenario)` runs a scenario with the host as a player and as a
dedicated server.

## Smaller pieces of the surface

- `Net.get_session()`: `Net`'s own `NetSession` while online (null otherwise); `Net.session_for(node)`
  is the session of a node's branch.
- `NetSession`: `unique_id()` (1 when hosting, the server-assigned id on a client after `connected`, 0
  before), `entity_count()` (entities in its table), `server_tick()` (the estimated server tick,
  fractional; the server's own tick on a server), `get_tick_rate()`, `get_api()` (gdnet's
  `NetMultiplayerApi` for the branch), `get_peer()` (the transport peer), `set_emulation(e)` /
  `get_emulation()`, static `packets_sent_total()` (every session of the process; 0 while nothing uses
  gdnet, gate G0). Test hooks start with `_debug_`: `_debug_protocol_salt`, `_debug_part_table_salt`,
  `_debug_perturb_property`, `_debug_extra_method` (properties) and `_debug_corrupt_held(node, prop,
  value)`; none is for games.
- `NetMultiplayerApi.get_inner()`: the wrapped `SceneMultiplayer` (stock RPCs and synchronizers run on
  it unchanged).
- `NetPipePeer`: `set_emulation(e)` / `get_emulation()` (applied at this end: `send_*` to what it sends,
  `receive_*` to what it receives), `in_flight()` (packets its emulators hold), and the counters
  `bytes_sent()`, `packets_sent()`, `bytes_received()`, `packets_received()`. The test hook
  `_debug_watch(bytes)` scans every packet this end receives for those bytes at any bit offset;
  `_debug_watch_hits()` and `_debug_watch_scanned()` count the packets that held them and the packets
  scanned (gate G9 uses it to show an `audience: owner` value never reaches another peer).
- `NetSession` seat surface (WP17): `key` and `dedicated` (set before `start`), `rejoin_timeout_s` (live;
  reads as the effective value, a value below -1 goes back to `Net.rejoin_timeout_s` and the setting),
  `seat(peer)`, `peer_of(seat)`, `my_seat()`, `seat_table()` (`{seat, peer, away}` each), `save_state()`,
  and `start(peer, root_path, state)`. `stats()` has `seats`, `seats_away` and `my_seat`.
- Project setting for seats: `gdnet/session/rejoin_timeout_s` (default 60).
- Project settings for rooms and interest (pages 9 and 11): `gdnet/levels/empty_timeout_s` (default 30),
  `gdnet/levels/spacing` (default 0 = levels keep their authored place), `gdnet/levels/keep_state`
  (default true), `gdnet/interest/filter_every_ticks` (default 6).
- `NetManager.get_managed_root()`: the node whose subtree it manages (itself, or the node given to
  `Net.manage()`).
- `NetVersion`: `version()` (the gdnet crate) and `core_version()` (netcore; equal in a consistent build).
