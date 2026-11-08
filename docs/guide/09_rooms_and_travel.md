# 9. Rooms and travel: several levels, players moving between them

A game with more than one level needs three things from the network layer: the server keeps every level
someone is in, each client loads only its own level, and a peer receives only the entities of the level it
is in. gdnet does this with *rooms* (`docs/PLAN_NETCODE.md` 2.7, 2.11, 3.1, WP13). The worked example is
the Doors demo, `demos/doors/README.md`.

## Rooms

Every `NetManager` has a `room`. Leave it empty and it is the scene the manager was saved in (its owner's
`scene_file_path`): a manager inside `res://levels/cave.tscn` is in the room `res://levels/cave.tscn`.
A manager with no scene (added by code) uses its path relative to the session's branch. Set it in the
inspector to group several managers into one room, or to `"*"`: a `"*"` manager is visible from every
room (scoreboards, a global weather entity).

A peer's room is the room of the manager holding its *player*. The first entity `Net.spawn_for`'d to a
peer is its player; `Net.set_player(peer, node)` says so explicitly (before or after the spawn).
`Net.set_room(peer, name)` puts a peer in a room whatever its player's (spectators, a lobby); an empty name
removes the override.

## Interest by room

A peer sees an entity when the entity's topmost entity is owned by that peer, or its manager's room is
the peer's room or `"*"`, or its manager has no room registered. The server keeps, per peer, the set of
entities it sent it (its *scope*) and filters by it: creates, destroys, moves, owner changes, snapshots,
the owner-state relay, hashes and broadcast calls. When a peer's room changes, or an entity moves to a
manager in another room, the server sends the difference:

* `Scope { leave }`: entities that left the peer's scope. **The client frees those replicas** (their
  `_net_despawning` runs and `entity_despawning` fires, as for a destroy); they come back with a fresh
  `Create`, current state included, if the peer sees them again.
* a burst of `Create`s, parents first, for the entities that entered.

A late joiner's burst covers its room only. Before a peer has a room it sees everything, as in a game with
one level, *unless the session uses levels*: once the server called `load_level` or `travel`, a peer without
a room sees only `"*"` managers (so a joining client never receives a level it is about to leave).

The keyed state (`Net.state`, page 7) is global: chat and scores cross rooms.

`NetSession.stats()`: on a client `entities_in_scope` is what it holds; on the server it is every entity,
and `entities_in_scope_by_peer` has each peer's count. `room` (a client's own room), `rooms` (entities per
room name), `levels`, `travelling` (server: peers that have not acknowledged a travel; client: 1 from a
`Travel` until one of its entities is in that level), `travels`, `scope_entered`, `scope_left`. The meter
shows them on its `rooms` line; `overlay_rows()` gain `room`.

## The World pattern

Levels go under one node the game owns, the *World*:

```gdscript
extends Node2D            # world.gd, a node in the main scene

func _ready() -> void:
	Net.set_world(self)   # levels load under this node (in its branch)
```

Without one they go under the session's branch root. A level node is named after its scene file
(`cave.tscn` becomes `cave`), so `World/cave/NetManager` has the same path, and therefore the same manager
key, on every peer.

Server side:

```gdscript
var level: Node = Net.load_level("res://levels/hub.tscn")   # loads it, or returns it when loaded
Net.levels()                                                # ["res://levels/hub.tscn"]
Net.unload_level("res://levels/hub.tscn")                   # destroys its entities, frees it
Net.spawn_for(peer_id, PLAYER, level.get_node("NetManager"))
```

`Net.load_level`, `unload_level`, `levels` and `set_room` act on `Net`'s own session; the same methods exist
on `NetSession`, and `Net.session_for(node)` returns the session of a node's branch (the headless tests run
several peers in one process). A client never loads levels itself: the server tells it.

## Travel

```gdscript
# door.gd (an Area2D), on the server:
func _on_body_entered(body: Node2D) -> void:
	if multiplayer.is_server() and body is Player and owner.is_ancestor_of(body):
		Net.travel(body, "res://levels/cave.tscn", "Entrance")
```

`Net.travel(player, scene_path, spawn_point = "")` is queued and applied at the next network tick (so it
is safe inside `body_entered`, where the engine refuses to move a body). Then, on the server:

1. the target level is loaded under the World if it is not, and its managers register their rooms;
2. the player's entity (and every entity below it) moves from its manager to the level's manager, at the
   node named `spawn_point` inside the level (a `Marker2D`/`Marker3D`), else at the level's origin;
3. `_net_travelled(from, to)` runs on its parts and `travelled(player, from, to)` fires on `Net` /
   `NetSession`, with the two room names;
4. the owner's room changes: its client gets `Travel` (the level's scene path and room), the scope
   change (`Scope` leaves for the old room, `Create`s for the new one), then the `Reparent` of the player.

The client, on `Travel`, instantiates the level under its World at once and answers `TravelAck`; the
`Reparent` waits until the destination manager exists, then moves the player's node into it and fires the
same `_net_travelled` and `travelled`; prediction and interpolation of the moved entities restart (the
owner re-tracks prediction at the next tick and rolls back to the server's position, replicas start new
sample buffers). Afterwards the client frees every other level that holds none of its entities.

Peers in the old room see the player leave (a `Scope` leave: the replica is freed); peers in the new room
see it arrive (a `Create`). Peers that see both rooms (none, unless the player's manager is `"*"`) get the
`Reparent`.

The server unloads a level `empty_level_timeout` seconds after its last player left (no player entity in
any of its managers): `NetSession.empty_level_timeout`, else `Net.empty_level_timeout`, else the project
setting `gdnet/levels/empty_timeout_s` (default 30). Its entities are destroyed first. The next `travel` (or
`load_level`) loads it again, with fresh entities.

**Moving an entity between managers by hand** (`remove_child` + `add_child`) is still a destroy and a
create; `Net.travel` is the move.

**One physics space.** A server holding several levels has them all in one `World2D`, overlapping at the
origin. Doors and other areas should ignore bodies of other levels (`owner.is_ancestor_of(body)` above),
and level geometry that blocks players needs a collision layer per level, or levels placed apart.

## A headless server

```
godot --headless --path . demos/doors/main.tscn -- --server
```

Everything after `--` reaches the game through `OS.get_cmdline_user_args()`; the demo's `main.gd` hosts
without a local player when it sees `--server`. To try a server and two clients from the editor: *Debug >
Customize Run Instances...*, *Enable Multiple Instances*, 3 instances, launch arguments `-- --server` on
the first; run the scene and press Join in the other two windows.

## Limits

* Rooms are all or nothing per manager: there is no distance-based interest yet (`radius`).
* A `Travel` carries the scene path; the client must have the same scene at the same path (as for every
  networked scene).
* A level holding a player owned by the server (the host's own player) is never empty.
