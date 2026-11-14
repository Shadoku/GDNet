# 13. Seats and rejoining: players who drop, saved sessions

A connection drops: a crash, a router, a laptop lid. Without seats the player's entities would go back to
the server and the player would come back as a stranger. This page covers seats and seat keys, what
happens while a player is away and when it comes back, the keyed rule `"seat"`, and saving a session to
resume it later (`docs/PLAN_NETCODE.md` 12.3, D22, WP17). The Cards demo (`demos/cards/`) is the worked
example.

## Seats and keys

A *seat* is a player's number for the session: 1 is the host's player on a listen server, otherwise the
first client; the others count up in the order they join. A peer id changes with every connection; a
seat does not.

Each client joins with a *seat key*, a string. The server maps keys to seats: a key it has not seen gets
the next seat, a key whose seat is away takes that seat back.

```gdscript
Net.join("10.0.0.2", 7777)               # joins with Net.key
Net.join("10.0.0.2", 7777, "slot-2")     # joins with this key, for this session only
Net.key = "name:Alice"                   # every later session of Net joins with it
```

`Net.key` is a random string kept for the life of the process unless the game sets one (`""` picks a new
random key). A crash therefore loses the seat unless the game passes a key that survives a restart: an
account id, a save slot, a name the player types. Over Steam the connect layer does this for you: the key
is `"steam:<Steam id>"` (page 12). A test's `NetSession` has its own `key`, random per session object.

Keys are identifiers, not passwords: nothing checks them, and anyone who sends another player's key gets
that player's seat while it is away (plan 12.1). A key whose seat is still connected is refused with a
reason that names the seat, and a client that joins with an empty key (gdnet never sends one) gets a seat
nobody can take back.

`Net.dedicated = true` before `Net.host` or `Net.use_peer` makes a server that holds no seat (its first
client is seat 1); a listen server's host player is seat 1. `Net.offline()` always plays as seat 1.

## Queries, signals and stats

| | |
|---|---|
| `Net.seat(peer)` | the peer's seat (also the last peer of a seat that is away); 0 without one |
| `Net.peer_of(seat)` | the peer on that seat; 0 while it is away or unknown |
| `Net.my_seat()` | this process's seat; 0 on a dedicated server |
| `Net.seat_table()` | every seat, ascending, as `{seat, peer, away}` |
| `peer_dropped(seat)` | a seated client disconnected and its seat is held |
| `peer_rejoined(seat, peer)` | the seat's player is back, as `peer` |
| `seat_released(seat)` | the seat was given up (its window ended, or the window is 0): its entities went to the server |

The queries answer on every peer, and the signals fire on the server and on every client. The rest of
the API keeps taking peer ids (`spawn_for`, `set_owner`, the verbs). `NetSession` has the same members,
and its `stats()` has `my_seat`, `seats` (taken seats, connected or away) and `seats_away`; the overlay
shows `#<net id> seat <seat> away` on an away seat's entities (page 8).

## The rejoin window

A seat whose client disconnects is held for the *rejoin window*, in seconds. Three places set it, the
first that is set wins:

| | |
|---|---|
| `NetSession.rejoin_timeout_s` | one session (a value below -1 goes back to the next line) |
| `Net.rejoin_timeout_s` | every session whose own value is unset (below -1 goes back to the setting) |
| `gdnet/session/rejoin_timeout_s` | the project setting, default 60 |

0 releases a seat at once: the entities go to the server and no `peer_dropped` fires (v1's behaviour).
-1 holds seats for the whole session, for a turn-based game. The value is live, and reads back as the
effective value.

## While a player is away

Its entities stay in the world, owned by its old peer id (`Net.get_owner_of` and `Net.seat(old_id)` still
answer), and nothing is destroyed:

- the server simulates them with a **neutral input frame** (no action pressed), so a dropped player stands
  still; this is not counted in `inputs_repeated`;
- `client_owned` entities freeze, since their owner simulated them and the server does not;
- the levels the seat's player is in stay loaded on the server past `empty_level_timeout`.

The game sees `peer_dropped(seat)` and `Net.peer_of(seat) == 0`, and decides what that means: a card game
waits, an action game might let the others play on. When the window ends, `seat_released(seat)` fires and
the entities go back to the server; they stay in the world. The key then sits down like a new player.

## What a rejoin restores

When the key comes back inside the window, the new peer id takes the seat's place everywhere the server
knew the old one:

- ownership of its entities (`SetOwner` to every peer that has them; `owner_changed` fires) and its player;
- its rooms, the rooms it was added to and a primary room override (page 9), and its focus node (page 11);
- the `visible_to` / `Net.show_to` peer lists naming it, the decisions of keyed-state views, and the
  values of `audience: owner` properties it receives;
- then the client gets what a late joiner gets: the `Travel`s to its levels, the entities of its scope
  and the keyed state it may see.

`peer_rejoined(seat, peer)` fires once that is done. Under 30 ms each way and 10 % loss, a rejoined
client held all four entities of the test 8 ticks after connecting.

Not restored: keyed paths written under the `"owner"` rule, which name the old peer id. Use `"seat"`
for anything a player keeps across a drop:

```gdscript
Net.state.allow("hand/", "seat")    # seat 2 may write hand/2 (and hand/2/...), also after a rejoin
```

A view filter that tests a seat (`Net.seat(peer) == 2`) admits the new connection at the next tick; one
that tests a peer id needs updating in `peer_rejoined`.

## Saving and resuming a session

On the server (or offline), `Net.save_state()` returns the session as bytes; the game writes them where
it likes. Hosting from them starts the same session again:

```gdscript
var bytes := Net.save_state()                 # NetSession.save_state() in a branch
Net.host(7777, bytes)                         # also Net.offline(bytes), Net.use_peer(peer, bytes)
```

A state that does not decode is refused (`ERR_INVALID_DATA`, an error says why) and nothing starts.

| in the state | not in it |
|---|---|
| the keyed state and its `allow` rules | views: they are code, so declare them again after starting |
| the seats and their keys, and the host's seat | entities under managers outside any level |
| every level: the loaded ones captured now, unloaded ones from their kept state (page 9) | the game's own data outside gdnet (save it beside the bytes) |

Started from a state, the server holds every saved seat as *reserved*: it never expires, whatever the
rejoin window. The host takes the saved host's seat whatever its key; each client that joins with the key
it had takes its seat. Entities a seat owned come back owned by the server (as a level loads) and go to
that seat's peer once it connects. A game with a run to resume stores the bytes beside its own state, as
the Cards demo stores `{session, game}` in one file after every card.

## With the connect layer and Steam

`NetConnect` (page 12) joins over Steam with the key `"steam:<id>"`, and its `key` property sets one key
for every transport. Its `state` property is the state `host_enet`, `host_steam` and `offline` start from.
A player who comes back before the server noticed the old connection drop (over ENet that can take
seconds) is refused because the seat is still connected; `NetConnect` tries the join again `join_retries`
times, `retry_delay_s` apart. A game that calls `Net.join` itself retries the same way on a
`disconnected` reason containing "already in use".

## Limits

- There is no way for a new connection to take over a seat whose old connection is not yet noticed as
  dropped; the client retries (above).
- Keys are not verified (plan 12.1), and an empty key's seat cannot be taken back.
- While a seat is away its entities name the old peer id as owner; code that stored peer ids should
  listen to `peer_rejoined` (or store seats).
- A `visible_to` peer list keeps a departed peer's id; a new peer reusing that id would see those
  entities (page 11).
- No host migration: when the host goes, the session ends.
- A session state with one level is about 1.3 KB; levels and keyed state dominate its size.
