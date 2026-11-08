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

# A client.
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
| `connected` | this side is in the session: a client at `Welcome`, a host at its first tick |
| `disconnected(reason)` | the session ended on its own (refused, server lost); `leave()` emits nothing |
| `tick(tick)` | once per physics tick while online |
| `Net.tick()` | the current tick (a client adopts the server's at `Welcome`) |
| `Net.peers()` | remote peers that finished the handshake (a client sees `[1]`) |
| `Net.rtt(id)` | smoothed round-trip time in milliseconds, `-1` before the first sample |
| `Net.is_online()` | from `host`/`join`/`offline`/`use_peer` until `leave()` or a disconnection |
| `Net.tick_rate` | physics ticks per second |

The tick is the physics tick: gdnet runs its loop in `_physics_process`, never on its own clock, so
`move_and_slide()` and your own `_physics_process` stay in step with it.

One GDScript detail: the method `tick()` and the signal `tick` share a name, and Godot resolves `Net.tick`
to the method. Connect to the signal by name:

```gdscript
Net.connect("tick", func(t: int) -> void: print("tick ", t))
# or: Signal(Net, "tick").connect(...)
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
