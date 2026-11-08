# 7. Authority: owners, shared state, keyed state

**The server is authoritative unless you say otherwise.** Every entity has one owner, the server by
default; the server's copy is the truth, clients show it. This page covers owners and how they move,
nested entities, the opt-in shared authority of `client_owned` entities (and why it trusts a client),
rejected writes, and the keyed state `Net.state` with its permissions and views (`docs/PLAN_NETCODE.md`
1 rung 2, 2.1 "Who simulates" and "Spawn", 2.4, 2.6, D9, WP7).

## Owners

`owner_peer` is a peer id (1 = the server). It is set when the entity is spawned (`NetEntity.owner_peer`,
`Net.spawn_for(peer, scene, parent)`, or `Net.set_owner(node, peer)` before `add_child`) and changed later
by the server with `Net.set_owner(node, peer)`. What the owner means:

| | server-owned (default) | owned by client C |
|---|---|---|
| who runs `_physics_process` | the server | the server (with C's input) and C (predicting, page 4) |
| whose input is used | the real `Input` | C's input frames; frames from anyone else are dropped |
| who may call `sync` / `owner` / `others` / `peer` on it | the server | C (and anyone `_net_allow` admits, page 6) |
| whose state is the truth | the server's | the server's (C's prediction is corrected toward it) |

Queries: `Net.get_owner_of(node)`, `Net.is_owner(node)`, `Net.is_authority(node)` (the server).

Every change is one `SetOwner` message to every peer, and on every peer, once:

```gdscript
func _net_owner_changed(old: int, new: int) -> void:   # on any part of the entity
	$Label.text = "P%d" % new

$NetEntity.owner_changed.connect(func(old, new): ...)   # the NetEntity's signal
Net.owner_changed.connect(func(node, old, new): ...)    # (NetSession.owner_changed in a fixture branch)
```

An owner change moves simulation with it: the new owner starts predicting (fresh history), the old one
becomes a replica that interpolates (its buffers reset), and the server takes the new owner's input. A
client that leaves gives its entities back to the server; they stay in the world.

## Nested entities inherit their owner

A nested entity (a split instance inside another entity, page 2) takes the owner of the entity around it
when it is created:

```
Player.tscn (owner 5, from Net.spawn_for)
└── Shield.tscn (NetEntity)           # owner 5 too
└── Pet.tscn (NetEntity, inherit_owner = false)   # owner 1: its own owner_peer
```

An entity keeps its own owner when its `NetEntity` has `inherit_owner = false` or an `owner_peer` other than
1, or when `Net.set_owner` / `spawn_for` named one. Inheritance happens once, at creation: changing the
outer entity's owner later does not move the nested ones (set them too, or use `transfer`).

## Transfer: asking for ownership

`NetEntity.transfer` decides whether a client may move ownership itself:

| `transfer` | `Net.request_owner(node)` from a client |
|---|---|
| `fixed` (default) | refused: only the server's `Net.set_owner` moves it |
| `request` | the current owner decides (below) |
| `take` | granted at once (a ball anyone may pick up) |

```gdscript
var granted: bool = await Net.request_owner($Ball).completed
Net.release_owner($Ball)       # the owner gives it back to the server
```

`request_owner` returns a `NetCall`; `completed` carries true when granted (already the owner, or no
session: true at once). For `request`, the owner is asked: on a server-owned entity the server, on a
client-owned one that client. The parts' optional hook answers:

```gdscript
func _net_grant_owner(requester: int) -> bool:
	return not holding_on        # every part that defines it must say yes
```

Without the hook the answer is **yes for a server-owned entity, no for a client-owned one** (a client keeps
what it has unless its script lets go). An owner that does not answer within 5 s counts as no.

## `client_owned`: shared authority (read this before using it)

Prediction (page 4) keeps the server authoritative: the owner guesses, the server decides, the owner is
corrected. Some things do not need that: a cosmetic pet, a client-side placement preview, a physics toy in a
casual co-op game, a spawn a client makes itself. `NetEntity.client_owned = true` switches an entity to
**owner authority** (Fusion's Shared mode, Roblox's network ownership):

- the owner runs its `_physics_process` (no prediction, no rollback) and its state is the truth;
- the owner sends its state (hot every tick it changes, cold when polled) to the server, which applies it,
  shows it on its own copy (not simulating it) and relays it to every other peer, who interpolate it;
- the owner never receives its own state back, and never interpolates its own copy.

```gdscript
# box.gd (NetEntity child: client_owned = true)
func _physics_process(delta: float) -> void:
	position += speed * delta      # plain single-player code, on the owner only
```

> **Trust warning (D9).** A `client_owned` entity's state comes from a client and the server does **not**
> check it: a modified client can teleport it, give it any value, or stop it. Use it only for entities whose
> state you are willing to let that client decide. Anything that decides a game (health, score, who won)
> stays server-authoritative: give the player ownership for prediction, keep `client_owned` off, and let
> the server validate calls with `_net_allow`.

`spawn_policy = owner_allowed` (page 2) makes every entity a client spawns `client_owned` by its spawner:
the client simulates it and its movement reaches everyone through the server. Ownership of a
`client_owned` entity moves like any other (`set_owner`, `transfer`); whoever owns it is the one trusted.

## Rejected writes

A client may only write state it owns (a `client_owned` entity's). When code on a client changes a
replicated property of a replica it does not own, gdnet treats it as a write: the client sends it, the
server refuses it, and the client **puts the server's value back** (Normcore's rule):

```gdscript
$Crate.z_index = 7            # on a client that does not own the crate
# a few ticks later: z_index is back to the server's value, and
Net.write_rejected.connect(func(node, sender): ...)   # on the writer: sender is itself
```

On the server, `write_rejected(node, sender)` names the writer (useful for logging cheat attempts). What
counts: a replica's replicated properties that are not interpolated (interpolated ones are rewritten every
frame anyway) and that the engine does not change by itself (an `AnimatedSprite2D`'s `frame`, an
`AnimationPlayer`'s position, `playing`, `emitting`), polled every `cold_poll_ticks`; entities this client
owns, simulates (`simulate = everywhere`) or predicts are never checked. To change such a value, ask the
owner: `Net.sync(set_color, c)` or `await Net.server(...)`.

## Keyed state: `Net.state`

Not everything belongs to an entity: scores, chat, the round timer, a vote. `Net.state` is a reliable map
from paths to values, on every peer:

```gdscript
Net.state.set("score/" + str(peer_id), 10)        # server
var s: int = Net.state.get("score/5", 0)          # anyone
Net.state.changed.connect(func(path, value): refresh(path))
Net.state.erased.connect(func(path): ...)
Net.state.keys("score/")                          # sorted paths under a prefix
Net.state.has("round/timer"); Net.state.erase("round/timer")
```

Changes travel as deltas on the reliable channel; a peer that joins late gets the whole map it may see in
one message. Values are anything a call argument may be (no Objects, Callables or RIDs). Without a session
`Net.state` is a local map with the same API and signals (single player), and sends nothing. In a fixture
branch the same object is `NetSession.state`.

### Who may write

Only the server writes, until it opens a prefix:

```gdscript
Net.state.allow("chat/", "all")       # every peer may write chat/...
Net.state.allow("score/", "owner")    # peer 5 may write score/5 (and score/5/...), no other score
Net.state.allow("admin/", 7)          # one peer
```

The rules reach the clients, so a client's `set` on a path it may not write fails at once with an error and
sends nothing. An allowed client write applies on the writer immediately and reaches everyone through the
server; if the server refuses it anyway (the rules changed meanwhile), the writer's value is rolled back and
`Net.state.write_rejected(path)` fires.

### Who may read: views

```gdscript
Net.state.view("team/red/", func(peer: int) -> bool: return peer in red_team)
```

Paths under a view's prefix reach only the peers its filter admits. The server re-evaluates the filter every
tick: a peer that becomes admitted receives the paths, one that stops being admitted gets them erased. A
path under several views needs every one of them. Views decide delivery, not secrecy from the server.

## Summary of the API

| where | what |
|---|---|
| `NetEntity` | `owner_peer`, `transfer` (`fixed` / `request` / `take`), `client_owned`, `inherit_owner`, signal `owner_changed(old, new)` |
| any part | `_net_owner_changed(old, new)`, `_net_grant_owner(requester) -> bool` |
| `Net` (and `NetSession`) | `set_owner(node, peer)` (server), `request_owner(node) -> NetCall`, `release_owner(node)`, signals `owner_changed(node, old, new)`, `write_rejected(node, sender)`, `state` |
| `NetKeyedState` | `set`, `get(path, default)`, `erase`, `has`, `keys(prefix)`, `allow(prefix, who)`, `view(prefix, filter)`, signals `changed(path, value)`, `erased(path)`, `write_rejected(path)` |
| `NetSession.stats()` | `owner_changes`, `writes_rejected`, `writes_sent`, `owner_states_sent`, `owner_states_received`, `ownership_requests_granted`, `ownership_requests_refused`, `keyed_bytes_sent`, `keyed_bytes_received`, `keyed_writes_rejected`, `keyed_entries` |
