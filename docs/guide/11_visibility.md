# 11. Visibility: radius, who sees an entity, and private properties

Rooms (page 9) decide which level a peer receives. This page covers the finer rules inside a room
(`docs/PLAN_NETCODE.md` 12.2, WP16): a peer gets only the entities near it (*radius*), an entity can exist
only on some peers (*`visible_to`*), a property can reach only the entity's owner (*`audience`*), and a
script can decide per peer (*`visibility_filter`*).

These are game-logic tools: "the other player's hand is not on my screen", "far things are not sent". They
are not a security boundary. A peer that owns an entity always has it, and nothing here checks what a
client does with what it receives (12.1).

All four are decided on the server. A client never needs a line of code for them: an entity it may not
have is never created there, and when it may have it again it arrives with a fresh `Create` carrying the
server's current state.

## Radius

```gdscript
# Inspector: NetManager.radius = 800      (every topmost entity of this manager)
# or per entity: NetEntity.radius = 300   (0 = the manager's; negative = unlimited)
Net.configure(tree, {"radius": 300.0})    # an entity without a NetEntity
```

An entity with a radius is in a peer's scope only while its root is within that distance of the peer's
*focus*. It enters at the radius and leaves beyond 1.2 times it, so an entity at the edge is not created and
freed in turn. When it leaves, the client frees the replica as for a destroy (`_net_despawning` runs,
`entity_despawning` fires).

* Units are those of the scene: pixels in 2D, metres in 3D, as `interaction_radius` (page 10).
* The radius applies to topmost entities. A nested entity goes wherever the entity it sits in goes.
* The radius is read when the entity spawns. `NetEntity.radius` and `configure()` change it at run time.
* No radius anywhere (the default) costs nothing: the server reads no positions.

### Focus

A peer's focus is its player (page 9: the first entity `spawn_for`'d to it, or `Net.set_player`). For a
camera, a spectator or a cutscene, give another node:

```gdscript
Net.set_focus(peer_id, camera)   # any Node2D / Node3D on the server
Net.set_focus(peer_id, null)     # back to the player
```

`NetSession` has the same `set_focus`, and `get_focus(peer)` returns the node. **A peer without a focus
position gets no radius-limited entity**, the same way a peer without a room gets only `"*"` managers once
levels are in use. This keeps a joining client from receiving the whole world just before its player
spawns. A spectator therefore needs `set_focus`.

The peer's own entities ignore the radius: an entity whose topmost entity a peer owns is always in that
peer's scope.

## visible_to

```gdscript
# Inspector: NetEntity.visible_to = Owner    (all | owner | peers)
Net.show_to(card, PackedInt32Array([peer_a, peer_b]))   # visible_to = peers, with this list
Net.configure(card, {"visible_to": "owner"})            # "all", "owner" or an Array of peer ids
```

`owner` means only the entity's owner has it: a card in a hand. When the owner changes, the entity moves
with it. A peer list (`visible_peers` on the `NetEntity`) fits a card shown to one opponent. The owner
always has its own entity, whatever the list says. An entity hidden from a peer hides its children too: a
replica cannot exist without its parent.

## audience: a property only the owner receives

```gdscript
# NetEntity.state_options = {"face": {"audience": "owner"}}
# a profile entry:          {"name": "face", "audience": "owner"}
Net.configure(card, {"audience": {"face": "owner"}})
```

Every peer has the entity; only its owner receives `face`. The other peers keep the scene's value (`-1`
for an unknown card). The server leaves the value out of everything it sends them: snapshots, the state in
`Create` and in a late joiner's burst, and reliable fields. Hash telemetry (page 8) leaves the property out
entirely. When the owner changes, the new owner receives the current value; the old owner keeps the last
value it saw and hears no more changes.

`audience` is part of the property table, so both peers must declare it the same way (they do when the
same scenes and scripts run everywhere). Like other table options, `configure()` must come before the
scene's first spawn.

## visibility_filter

```gdscript
# NetEntity.visibility_filter = is_visible_to   (set from a script; a Callable is not exported)
Net.set_visibility_filter(scout, func(peer: int) -> bool:
	return teams.same_team(peer, scout_owner) or revealed)
```

The server calls the filter for every client peer every `gdnet/interest/filter_every_ticks` ticks (default
6), and at once when the filter is new or a peer joins or leaves. An entity whose filter returns false for
a peer leaves that peer's scope, and enters again when it returns true. Until its first result a new
entity with a filter is on no client. `Net.set_visibility_filter(node, Callable())` removes the filter.

Keep filters cheap: each evaluation runs once per filtered entity per peer.

## Which to use

| you want | use |
|---|---|
| far things not sent, a large world | `radius` on the manager, `set_focus` for spectators |
| an entity only its owner or a few peers have (a hand, a private quest item) | `visible_to` / `show_to` |
| everyone has the entity, one value is private (a face-down card) | `audience: owner` |
| a rule the others cannot express (fog of war, teams, line of sight) | `visibility_filter` |

The rules combine: an entity is in a peer's scope when the peer owns its topmost entity, or when its room,
its radius, its `visible_to` and its filter all allow it.

## Diagnostics

`NetSession.stats()` on the server: `interest_passes` (radius passes run), `interest_ms` and
`interest_ms_avg` (the pass's cost, last tick and per tick over the last second), `filter_evaluations`,
`radius_entities`, `radius_changes` (entries and exits of radius sets). `entities_in_scope_by_peer` counts
each peer's scope with every rule applied; on a client `entities_in_scope` is what it holds.
`NetSession.in_radius(peer)` lists the radius-limited entities inside a peer's radius.

A change of interest updates only the entities it concerns: when an entity enters or leaves a peer's radius,
or its `visible_to` or filter result changes, the server checks that entity and those below it for that
peer. A change of rooms rebuilds the peer's whole scope.

Measured (headless, release build; `test_g9_no_entity_beyond_one_point_two_r` prints it with
`--verbose`): 1,000 radius-limited entities on a 190 px grid over about 6,000 x 6,000 px, two clients
walking 8 px per tick, cost the server's interest pass 0.62-0.72 ms per tick on average and 1.1-3.4 ms at
most over 150 ticks (it reads every radius-limited root's position each tick). Rebuilding the whole scope on
every change, as before, cost 2.5-2.7 ms on average and 9-13 ms at most on the same scene.
