# 2. Entities: enrol a scene, spawn it everywhere

Rung 0 of gdnet is "enrol a scene, change nothing": put a `NetManager` in your level and everything you
spawn beneath it on the server appears on every client, under the same parent, with the same name. This
page covers what the manager enrols, how spawning, despawning and reparenting travel, who owns an entity,
and who may spawn. What an entity's properties do between spawn and despawn (positions, animation,
exported variables) is the next package's job: see "Not synced yet" at the end.

## The manager

```
Level (Node2D)
└── NetManager            # everything beneath is managed
    ├── Player.tscn       # an instance: an entity
    ├── Door.tscn
    └── Enemies (Node2D)  # a bare node: an entity of its class (with a warning)
        └── Slime.tscn ×3
```

`NetManager` is a plain `Node`; add it from the editor (it is a class like any other) or from code. It
does nothing until a session runs (`Net.host`, `Net.join`, `Net.offline`, `Net.use_peer`, or a
`NetSession` in tests): with no session, no packets are sent and no per-tick work happens (gate G0).

Every peer must have the same managers at the same paths: a manager is identified by its path relative to
the session's root (the scene tree's root for `Net`, the branch for a test `NetSession`). You may have
several managers in one level; each entity belongs to the manager it sits under.

## Entities and parts

* A **scene instance** under the manager (a node whose `scene_file_path` is set) is an **entity**.
* The nodes **inside** an instance are its **parts**, numbered by a depth-first walk of the scene. A part
  is (entity id, part index): no `NodePath` ever travels.
* A **nested instance** that is part of the outer scene is a part too, unless it has a `NetEntity` child
  or its scene is listed in the manager's `split_scenes`: then it is an entity of its own.
* A scene instance a script adds **at run time** inside an entity is an entity of its own (it is not in
  the outer scene, so it cannot be a part). A plain node a script adds inside an entity is ignored.
* A **bare node** under the manager, outside any instance (a `Sprite2D` made with `.new()`), is an entity
  of its class. gdnet warns once: scenes are the unit of composition, and a bare node replicates as its
  class name only.
* `Net.ignore(node)`, or the group `net_local`, keeps a node and everything below it out (call it before
  the node enters the managed subtree).
* `Net.manage(node)` makes `node` a managed root, as if it were a `NetManager` (it adds a hidden
  `__NetManaged` child). Call it on every peer, at the same path.

## Spawning, despawning, moving

On the server, spawning is what you already write:

```gdscript
var slime := preload("res://enemies/slime.tscn").instantiate()
slime.name = "Slime4"
$NetManager/Enemies.add_child(slime)   # every client gets Slime4 under NetManager/Enemies
slime.queue_free()                     # ... and loses it again
slime.reparent($NetManager/Cave)       # ... or sees it move
```

At the next physics tick the manager announces the entity in one reliable message (`Create`: its id,
scene, parent, name, owner and, from WP3 on, its initial state). A client loads the scene
(`load(path).instantiate()`), sets the name, adds it under the same parent and then calls `_net_spawned`.
A `Create` whose parent has not arrived yet waits for it (it is never dropped; an error says so after five
seconds). `queue_free()` sends `Destroy` (children of a destroyed entity go with it); moving a node within
managed space sends `Reparent`.

Content that is part of the level on every peer (instances present when the manager becomes ready, or
nested instances inside a spawned scene) is not instantiated twice: a client binds its own node to the
server's entity when the `Create` names the same parent, name and scene.

A client that joins late receives the whole entity table, parents first, right after its handshake.

### The same scene at the same path

The `Create` names the scene by a hash of its resource path, so every peer must have the scene at the same
path. Both peers compute the scene's part table (node paths and classes); if a client's table differs from
the server's, the client is disconnected with a reason that names the scene:

```
protocol mismatch: the part table of scene res://enemies/slime.tscn differs (client ..., server ...);
both peers need the same version of the scene at the same path
```

## Callbacks and signals

Any script on any part may define these; nothing needs to extend a gdnet class:

```gdscript
func _net_spawned(info: Dictionary) -> void:
	# info: net_id, owner, is_owner, is_authority
	if info.is_owner:
		$Camera2D.make_current()

func _net_despawning() -> void:
	$Explosion.restart()
```

`_net_spawned` runs on the server when the entity is announced (`is_authority` true) and on each client
after the replica entered the tree. `_net_despawning` runs on a client just before gdnet frees the replica,
and on the server when the removal is announced.

`Net.entity_spawned(node)` and `Net.entity_despawning(node)` fire once per entity on each side (also on the
manager, and on a test `NetSession`).

## Owners

Every entity has an owner peer: the server (1) unless you say otherwise.

```gdscript
var player := Net.spawn_for(peer_id, preload("res://player.tscn"), $NetManager)   # server
Net.set_owner(player, other_peer)                                               # server, later
Net.is_owner(player)       # true on the owning peer
Net.is_authority(player)   # true on the server
Net.get_owner_of(player)   # the owner's peer id
```

A `NetEntity` child's `owner_peer` sets the owner a scene starts with. In this version the owner is data
only (it travels with the entity and answers `is_owner`); prediction and input from the owner come with
WP4, ownership transfer with WP7.

## Who may spawn: `spawn_policy`

* `server_only` (default): only the server spawns. A client-side `add_child` of an entity under the
  manager is refused at the next tick: gdnet prints an error naming the node, emits
  `Net.spawn_refused(node)` and frees it.
* `owner_allowed`: a client may spawn. The entity is owned by that client, the server instantiates it too
  and relays it to every other client; the client may also destroy (and move) what it spawned. Set the
  same policy on every peer's manager.

A node a client adds before its session is connected counts as level content, not as a spawn.

## Not synced yet

This package makes spawning work. Between spawn and despawn nothing else replicates yet: positions,
velocities, animation, visibility and exported variables come with WP3 (profiles and state replication),
which fills the `Create`'s initial state too. A bare node therefore arrives as a default instance of its
class.
