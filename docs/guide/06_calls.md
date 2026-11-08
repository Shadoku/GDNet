# 6. Calls: behaviour across the network

State replication (pages 3-5) covers what an entity *is*; calls cover what it *does*: open a door, play an
attack animation, ask the server whether a purchase is allowed. In gdnet every public method of a managed
node can be called across the network through one verb, with nothing declared on either side. This page
covers the verbs and where each one runs, trust and `_net_allow`, `NetCallInfo`, awaiting a return value,
the argument rules, what happens in a replay, ordering, and migrating from `@rpc`
(`docs/PLAN_NETCODE.md` 1 rung 2, 2.1 "Auto-exposed methods", 2.4, D4, D14, WP6).

## Rung 2 in five lines

Single player: `open_door(7)`. Multiplayer: `Net.sync(open_door, 7)`.

```gdscript
func open_door(id: int) -> void:          # a plain method; nothing declared
	doors[id].open = true
	$Sfx.play()

Net.sync(open_door, 7)                     # runs on the server, then on every peer
var granted: bool = await Net.server(try_buy, item_id)   # the server's answer

func _net_allow(method: StringName, sender: int) -> bool:  # optional trust hook
	return method != &"give_gold" or sender == 1
```

The first argument is a `Callable` on a part of a managed entity: a script method (`open_door`), a method
of another part (`$Door.open_door`), or a built-in one (`$Anim.play`, `$Label.set_text`, `$Timer.start`).
Up to eight arguments follow; bound arguments count too (`Net.sync(open_door.bind(7))`), appended after the
given ones as `Callable.call` would. Without a session (single player, or a node in a branch nobody hosts)
a verb simply calls the method, so the same script runs offline unchanged.

## The verbs and where they run

| verb | called on the server | called on a client |
|---|---|---|
| `Net.sync(f, ...)` | runs at once, then on every peer that has the entity | goes to the server only; the server checks trust, runs it, then sends it to every peer, **the caller included** |
| `Net.server(f, ...)` | runs at once | runs on the server, which answers with the return value |
| `Net.owner(f, ...)` | runs at once when the server owns the entity, else on the owner | runs at once when the caller owns it, else via the server on the owner |
| `Net.others(f, ...)` | runs on every client (not on the server) | via the server: runs on the server and every other client, not on the caller |
| `Net.peer(id, f, ...)` | runs on peer `id` (at once for `id` 1) | runs at once for its own id, else via the server on `id` (1 = the server) |

`Net.sync_unreliable(f, ...)` is `sync` on the unreliable channel, for cosmetic effects that may be lost.
The same verbs exist on a `NetSession` (they serve the node's own branch either way).

**One execution per peer, authority first.** A client's `Net.sync` does *not* run on the caller first. The
server runs it, and only then does it reach every client, the caller among them, once each. Every peer
therefore sees the result of the server's run (a door that was already open, a purchase the server
refused) instead of a local guess. The price is a round trip before the caller sees its own call; the
entity's predicted state (page 4) is what makes movement immediate, calls are for discrete events.

## Trust: ownership and `_net_allow` (D14)

The server accepts a client's `sync`, `owner`, `others` and `peer` call only when the caller **owns the
entity** (`Net.spawn_for`, `Net.set_owner`), or when the part's script defines

```gdscript
func _net_allow(method: StringName, sender: int) -> bool:
```

and it returns true. A refused call runs nowhere: the server emits `call_refused(node, method, sender)`
(on `Net` and the `NetSession`) and the caller's `NetCall` fails with an error that says why. `Net.server`
calls are accepted from any peer: the method decides, and it can learn the sender from `NetCallInfo`.
A call that comes from the server is always applied on clients. Methods whose name begins with `_` are
never network-callable (`_net_allow`, `_ready`, private helpers): calling one is an error at the call site,
and a hash that names one is answered with an error.

## NetCallInfo: who called, and when

A method whose **last declared parameter is typed `NetCallInfo`** gets one appended:

```gdscript
func buy(item_id: int, info: NetCallInfo) -> bool:
	return gold_of(info.sender) >= price(item_id)
```

`info.sender` is the peer that made the call (1 = the server), `info.tick` that peer's tick when it made
it, `info.is_local` true when the call was made on this peer (the server's own call on the server; a
client's call when the server's broadcast brings it back to that client). Callers never pass it.

## Awaiting a return value

`Net.server(f, ...)` returns the `completed` signal of a `NetCall`, so awaiting it gives the server's return
value:

```gdscript
var granted: bool = await Net.server(try_buy, 3)
```

On an error (refused, unknown method, the entity was destroyed, no answer within 10 seconds, the session
ended) the value is `null`; the signal's object is the `NetCall`, which says why:

```gdscript
var done: Signal = Net.server(try_buy, 3)
var call: NetCall = done.get_object()
var granted = await done
if not call.ok:
	push_warning(call.error)
```

The other verbs return the `NetCall` itself: `ok`, `error`, `result` (the return value of a call that ran
on this peer), `suppressed`, `is_done()`, and the signal `completed(value)`. A client's call resolves when
the server has routed it (or refused it); a server's call resolves at once. `completed` is emitted
deferred, so an `await` written right after the call never misses it. A void method returns `null`.

## Arguments

| value | travels as |
|---|---|
| `null`, `bool`, `int`, `float`, `String` | itself |
| a `Resource` with a resource path | its path; the receiver `load()`s it |
| any other value that holds no object: vectors, colours, `StringName`, `NodePath`, packed arrays, arrays and dictionaries of such values | Godot's `var_to_bytes` encoding |
| an `Object` (a node, a `Resource` without a path), a `Callable`, a `Signal`, an `RID`, or an array or dictionary holding one | refused |

A refused argument is an error at the call site, printed and on the returned `NetCall`, and nothing is
sent. To refer to another entity, pass its net id (`Net.net_id(node)`) or a path the receiver can resolve.

## Replays: calls fire once

On the owner of a predicted entity, a rollback (page 4) runs `_physics_process` again for ticks it already
simulated, with `Net.is_fresh()` false. Every verb called there **does nothing** and returns a `NetCall`
already resolved with `ok` and `suppressed` true, so a `Net.sync` written in `_physics_process` is sent
once, on the fresh tick, never again from a replay. (Remember that the server runs a client-owned
entity's `_physics_process` too: code there that should call once per press belongs behind
`Net.is_owner(self)`, or should run only on the server.)

## Ordering

Calls travel reliably and in order on channel 0, with the entity actions: a call always arrives after the
`Create` of its entity, after any reparent made before it, and before a later destroy. Two cases need care
and are handled for you:

- **A call on an entity spawned in the same tick.** A spawn is announced at the start of the next tick (page
  2), so a call made right after `add_child` has no net id yet. gdnet holds it and dispatches it, on every
  peer including the server, right after the spawn is announced. The `Create` carries the state from before
  the call; then the call runs everywhere. (A client applies it after `_net_spawned`.)
- **A call for an entity that is gone.** A call that reaches a peer after its entity was destroyed is
  dropped silently and counted (`calls_dropped` in `NetSession.stats()`); a client's request gets an error.
  A call held behind a `Create` that has not arrived waits with it and is never reported as "node not
  found".

`sync_unreliable` calls skip both rules: they may be lost, and one that arrives before its entity is
dropped.

After a call runs on the server, the entity's cold properties are polled at the next tick, so an `@export`
the call changed reaches every replica at once as state too (a late joiner sees it).

## Diagnostics

`NetSession.stats()` gains `calls_sent`, `calls_received`, `calls_refused`, `calls_dropped`,
`calls_unknown_method`, `calls_timed_out`, `calls_pending`, and per-second `calls_sent_per_s`,
`calls_received_per_s`, `calls_refused_per_s`, `calls_dropped_per_s`. Both peers derive each part's method
table from the scene and its scripts; the table's hash travels with the scene's first spawn, so a peer
running a different version of a script is disconnected with a reason naming the scene and the part.

## Migrating from `@rpc`

| `@rpc` | gdnet |
|---|---|
| `@rpc("any_peer", "call_local") func open_door(id)` + `open_door.rpc(7)` | `func open_door(id)` + `Net.sync(open_door, 7)` (the owner may call it; `_net_allow` widens that) |
| `@rpc("any_peer") func request(x)` + `request.rpc_id(1, x)`, answered by a second RPC back | `var v = await Net.server(request, x)` |
| `rpc_id(peer, ...)` | `Net.peer(peer, f, ...)` |
| `multiplayer.get_remote_sender_id()` | a trailing `info: NetCallInfo` parameter, `info.sender` |
| node paths that must match on every peer, and RPCs that can arrive before the node exists | entity ids and part indices; calls wait behind their entity's spawn |
| `"unreliable"` | `Net.sync_unreliable(...)` |

Stock `@rpc` keeps working under gdnet's `MultiplayerAPI` wrapper (gate G1), so a project can move one
scene at a time.
