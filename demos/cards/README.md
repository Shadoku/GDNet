# Cards: a turn-based game with private hands, seats and saved runs

The third of gdnet's instructional demos (docs/PLAN_NETCODE.md 12.3, WP17). Two players each hold five
cards dealt from a shuffled deck of sixteen; they take turns putting one card on the table, three each,
and the higher total wins. The rules are deliberately thin: the demo is about the shape of a networked
card game, which is different from the shape of an action game. Read it top to bottom: every file of the
demo is listed here, in full (a test, `tests/integration/test_cards_demo.gd`, checks that these listings
are the files).

What a card game needs, and where this demo shows it:

| need | how | file |
|---|---|---|
| rules that do not care about the network | a plain `RefCounted`, run only on the server | `cards_game.gd` |
| a table made of UI | `Control` nodes, outside any `NetManager` | `table.tscn`, `table.gd` |
| a player acts | an intention sent as a call, `await Net.server(dealer.play_card, index)`; the server decides | `table.gd`, `dealer.gd` |
| everyone sees the table | the keyed state, `table/...` | `dealer.gd` |
| nobody sees another hand | the keyed state, `hand/<seat>/...`, behind a view that admits only that seat | `dealer.gd` |
| a player drops and comes back | seats held for the whole game; the game waits | `dealer.gd`, `table.gd` |
| stop now, go on tomorrow | `Net.save_state()` plus the rules' own state in a file; hosting from it | `dealer.gd`, `main.gd` |

The concepts are in `docs/guide/06_calls.md` (calls), `07_authority.md` (keyed state and views) and
`13_seats_and_rejoining.md` (seats, rejoining, saved sessions); this page is the worked example.

## Running it

* **In the editor**: open `demos/cards/main.tscn` and press *Run Current Scene* (F6). **Offline** plays
  against the house (the server plays seat 2's first card at once); **Host** hosts on port 7351, **Join**
  joins the address in the box. A game starts once two seats are at the table.
* **Over the internet, through Steam**: with GodotSteam installed (`tools/install_godotsteam.sh`) and the
  Steam client running, **Host on Steam** shows a join code for the other player to type into the same
  box (`docs/guide/12_playing_online.md`). Over Steam your seat key is your Steam id: close the game,
  start it again, join with the same code, and you are back on your seat with your hand.
* **Two players on one machine**: *Debug > Customize Run Instances...*, enable *Enable Multiple
  Instances*, set the count to 2 (or 3 with `-- --server` on the first, a dedicated server). Press **Host**
  in one window and **Join** in the other; type a name first if you want to close a window and sit down
  again (below).
* **A headless server**: `godot --headless --path . demos/cards/main.tscn -- --server`.
* **The tests**: `python3 tools/check.py --only integration --filter cards` runs whole games headless, as a
  host-player and as a dedicated server, with a dropped and rejoined player, the packets scanned for
  other players' cards, and a saved game hosted again; `--only unit --filter cards_game` tests the rules.

Click a card in your hand to play it. Close a player's window in the middle of a game: the other window
says "Waiting for seat 2..." and the game stops; start that player again, join, and it goes on.

## The rules: `cards_game.gd`

Everything about the game itself is in one `RefCounted` with no node, no signal and no network: the deck,
the deal, whose turn it is, what a play does, the score, the end. The single-player version of this game
would use exactly this file, and the unit tests (`tests/unit/test_cards_game.gd`) use it alone, in
milliseconds. The deal takes its random source as an argument, so a test that seeds it knows every hand.

`play(seat, index)` returns `""` when the card was played and a reason otherwise, and changes nothing
when it refuses. That return value is all the networking the rules ever see: the dealer passes it back
to the player who asked.

```gdscript file=demos/cards/cards_game.gd
class_name CardsGame
extends RefCounted
## The rules of the card game, with no node and no network: two seats, five cards each from a shuffled
## deck of sixteen, seat 1 first, one card each turn, three rounds; the higher total wins. Only the
## server holds one (the dealer); the unit tests use it alone.

## The deck, weakest first: a card is worth its place in this list, 1 to 16.
const DECK: Array[String] = [
	"Ash Wren", "Brass Newt", "Cinder Moth", "Dusk Hare", "Ember Fox", "Frost Lynx", "Gale Heron",
	"Hollow Elk", "Iron Boar", "Jade Viper", "Kiln Bear", "Lumen Owl", "Moss Golem", "Night Stag",
	"Opal Drake", "Prism Wyrm",
]
const HAND_SIZE := 5
const ROUNDS := 3

## Each seat's cards, seat 1 first.
var hands: Array = [[], []]
## The cards played so far, in order, as [seat, card].
var table: Array = []
## Each seat's total, seat 1 first.
var scores: Array = [0, 0]
## The seat whose turn it is.
var turn: int = 1


## A new game dealt from a deck shuffled by `rng`.
static func deal(rng: RandomNumberGenerator) -> CardsGame:
	var deck: Array = []
	deck.append_array(DECK)
	for i in range(deck.size() - 1, 0, -1):
		var j := rng.randi_range(0, i)
		var swap: Variant = deck[i]
		deck[i] = deck[j]
		deck[j] = swap
	var game: CardsGame = new()
	game.hands = [deck.slice(0, HAND_SIZE), deck.slice(HAND_SIZE, HAND_SIZE * 2)]
	return game


## A game from `to_dict()`'s dictionary.
static func from_dict(d: Dictionary) -> CardsGame:
	var game: CardsGame = new()
	game.hands = (d["hands"] as Array).duplicate(true)
	game.table = (d["table"] as Array).duplicate(true)
	game.scores = (d["scores"] as Array).duplicate()
	game.turn = int(d["turn"])
	return game


static func value_of(card: String) -> int:
	return DECK.find(card) + 1


## The cards `seat` holds.
func hand(seat: int) -> Array:
	return hands[seat - 1]


## `seat` plays the card at `index` of its hand. Returns "" when it was played, otherwise why not (and
## nothing changed).
func play(seat: int, index: int) -> String:
	if is_over():
		return "the game is over"
	if seat != turn:
		return "it is seat %d's turn" % turn
	var cards: Array = hands[seat - 1]
	if index < 0 or index >= cards.size():
		return "there is no card %d in your hand" % index
	var card: String = cards.pop_at(index)
	table.append([seat, card])
	scores[seat - 1] += value_of(card)
	turn = 3 - seat
	return ""


func is_over() -> bool:
	return table.size() >= ROUNDS * 2


## The seat with the higher total once the game is over; 0 before, or for a draw.
func winner() -> int:
	if not is_over() or scores[0] == scores[1]:
		return 0
	return 1 if scores[0] > scores[1] else 2


## Everything the game needs to go on, for a save file.
func to_dict() -> Dictionary:
	return {"hands": hands.duplicate(true), "table": table.duplicate(true), "scores": scores.duplicate(), "turn": turn}
```

## The title screen: `main.tscn` and `main.gd`

`main.tscn` holds the table (hidden until a session runs), a row with a name field and a New game button,
and gdnet's shared title screen, `addons/gdnet/connect/connect_menu.tscn` (the connect layer, guide page
12), the same menu the Doors and Ballpit demos use.

```text file=demos/cards/main.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/cards/main.gd" id="1_main"]
[ext_resource type="PackedScene" path="res://demos/cards/table.tscn" id="2_table"]
[ext_resource type="PackedScene" path="res://addons/gdnet/connect/connect_menu.tscn" id="3_menu"]

[node name="Main" type="Control"]
layout_mode = 3
anchors_preset = 15
anchor_right = 1.0
anchor_bottom = 1.0
script = ExtResource("1_main")

[node name="Table" parent="." instance=ExtResource("2_table")]
visible = false
layout_mode = 1

[node name="Seat" type="HBoxContainer" parent="."]
layout_mode = 0
offset_left = 16.0
offset_top = 16.0
offset_right = 416.0
offset_bottom = 48.0

[node name="Name" type="LineEdit" parent="Seat"]
layout_mode = 2
size_flags_horizontal = 3
placeholder_text = "your name (optional)"

[node name="NewGame" type="Button" parent="Seat"]
visible = false
layout_mode = 2
text = "New game"

[node name="Menu" parent="." instance=ExtResource("3_menu")]
title = "Cards"
port = 7351
```

`main.gd` does three things before the menu starts a session:

* **The name is the seat key.** Every client joins with a *seat key*, and the server gives a key back
  the seat it had. The connect layer's default key is the Steam id over Steam, and over ENet a random key
  that lasts until the game closes. A name typed here becomes the key on every transport
  (`menu.connector.key`), so a player who closes the game and starts it again types the same name and sits
  down on the same seat. Keys are identifiers, not passwords: nothing checks them (plan 12.1).
* **A saved game is resumed by hosting.** When the save file exists, its session part goes into
  `menu.connector.state`, so Host, Host on Steam and Offline all start from it (they call `Net.host(port,
  state)`, `Net.use_peer(peer, state)` and `Net.offline(state)`); the game part goes to the dealer.
  New game forgets the file.
* **Offline means the house.** `menu.connector.transport()` says how the session started; offline, the
  dealer plays seat 2 itself.

Once the menu's `started` fires, `_start()` starts the dealer and the table. As in every demo, the code
after that is the same for every transport, and `--server` makes a dedicated server (`Net.dedicated`:
the server holds no seat, so the first two clients are seats 1 and 2).

```gdscript file=demos/cards/main.gd
extends Control
## The title screen is gdnet's connect menu (`addons/gdnet/connect/`): Host, Host on Steam, Join (an
## address or a join code) or Offline (against the house). Above it, a name to sit down with and, when a
## saved game exists, New game. `--server` after `--` on the command line starts a dedicated server:
## `godot --headless --path . demos/cards/main.tscn -- --server`.

const SAVE := "user://cards_save.bin"

@onready var table: CardsTable = $Table
@onready var menu: NetConnectMenu = $Menu
@onready var _name: LineEdit = $Seat/Name
@onready var _new_game: Button = $Seat/NewGame

var _saved: Dictionary = {}


func _ready() -> void:
	menu.started.connect(_start)
	_name.text_changed.connect(_use_name)
	_new_game.pressed.connect(_forget_save)
	_saved = CardsDealer.read_save(SAVE)
	_new_game.visible = not _saved.is_empty()
	if not _saved.is_empty():
		menu.connector.state = _saved["session"]  # hosting goes on with the saved game
	if "--server" in OS.get_cmdline_user_args():
		Net.dedicated = true
		menu.host()


## A name is the seat key: type the same one after a restart to sit down again. Empty: over Steam the
## Steam id, over ENet a key that lasts until the game closes.
func _use_name(text: String) -> void:
	menu.connector.key = "" if text.strip_edges().is_empty() else "name:" + text.strip_edges()


func _forget_save() -> void:
	DirAccess.remove_absolute(ProjectSettings.globalize_path(SAVE))
	_saved = {}
	menu.connector.state = PackedByteArray()
	_new_game.hide()


func _start() -> void:
	$Seat.hide()
	table.show()
	table.dealer.save_path = SAVE
	table.dealer.house = menu.connector.transport() == "offline"
	table.dealer.start(_saved.get("game", {}))
	table.start()
```

## The table: `table.tscn` and `table.gd`

The card table is a `Control` with labels and a row of buttons. None of it is networked: it sits under
no `NetManager`, gdnet never sees it, and it is the same on the server and on every client. Only its
`NetManager` child, with one node in it, the dealer, is managed.

```text file=demos/cards/table.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/cards/table.gd" id="1_table"]
[ext_resource type="PackedScene" path="res://demos/cards/dealer.tscn" id="2_dealer"]

[node name="Table" type="Control"]
layout_mode = 3
anchors_preset = 15
anchor_right = 1.0
anchor_bottom = 1.0
script = ExtResource("1_table")

[node name="Box" type="VBoxContainer" parent="."]
layout_mode = 0
offset_left = 16.0
offset_top = 16.0
offset_right = 816.0
offset_bottom = 216.0

[node name="Status" type="Label" parent="Box"]
layout_mode = 2

[node name="Scores" type="Label" parent="Box"]
layout_mode = 2

[node name="Played" type="Label" parent="Box"]
layout_mode = 2
autowrap_mode = 3

[node name="Hand" type="HBoxContainer" parent="Box"]
layout_mode = 2

[node name="NetManager" type="NetManager" parent="."]

[node name="Dealer" parent="NetManager" instance=ExtResource("2_dealer")]
```

```text file=demos/cards/dealer.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/cards/dealer.gd" id="1_dealer"]

[node name="Dealer" type="Node"]
script = ExtResource("1_dealer")
```

Why not make the cards entities? Because a card game's state is small, changes only when someone acts,
and is mostly *about* the players (whose turn, whose hand), not things that move. Entities are for
things with a position and a lifetime in the world; the keyed state (guide 07) is a reliable map of paths
to values that every peer has, which is what a table is. The UI reads it and redraws: `refresh()` runs
whenever any path changes.

**Intentions as calls.** A click on a card does not change anything locally. It sends the intention to
the server and waits for the answer:

```gdscript
var answer: Variant = await Net.server(dealer.play_card, index)
```

`Net.server` runs `play_card` on the server (at once when this peer is the server) and returns its return
value, `""` or the reason for a refusal ("it is seat 1's turn"). A call needs a managed node to aim at,
which is why the dealer sits under a `NetManager` while the Controls do not. If the play was taken, the
table changes for everyone through the keyed state a moment later; the caller needs no special case.

`seat()` is `Net.my_seat()` for this branch: 1 or 2 at the table, 0 on a dedicated server, 3 and up for
anyone else who joins (they watch). A player's own hand is `hand/<seat>/cards`; the other hand is simply
not in this peer's keyed state.

```gdscript file=demos/cards/table.gd
class_name CardsTable
extends Control
## The card table: plain Controls, under no NetManager. It draws what the keyed state says (the table
## for everyone, `hand/<seat>/cards` for this player only) and sends a click on a card to the dealer
## as an intention, `Net.server(dealer.play_card, index)`; the server decides.

@onready var dealer: CardsDealer = $NetManager/Dealer
@onready var _status: Label = $Box/Status
@onready var _scores: Label = $Box/Scores
@onready var _played: Label = $Box/Played
@onready var _hand: HBoxContainer = $Box/Hand

var _session: NetSession


## Called once the session runs, on every peer.
func start() -> void:
	_session = Net.session_for(self)
	_session.state.changed.connect(func(_path: String, _value: Variant) -> void: refresh())
	_session.state.erased.connect(func(_path: String) -> void: refresh())
	refresh()


## This player's seat: 1 or 2 at the table, 0 for a dedicated server, 3 and up for someone watching.
func seat() -> int:
	return _session.my_seat()


## The cards in this player's hand, as far as this peer knows them.
func hand() -> Array:
	return _session.state.get("hand/%d/cards" % seat(), [])


## Plays the card at `index` of this player's hand. Returns "" when the server took it, otherwise why
## not (also shown on the status line).
func play(index: int) -> String:
	var answer: Variant = await Net.server(dealer.play_card, index)
	var why: String = "no answer from the server" if answer == null else String(answer)
	if not why.is_empty():
		_status.text = why
	return why


## Redraws the table from the keyed state.
func refresh() -> void:
	var state := _session.state
	var scores: Array = state.get("table/scores", [0, 0])
	var turn: int = state.get("table/turn", 1)
	var away: Array = state.get("table/away", [])
	_scores.text = "Seat 1: %d    Seat 2: %d" % [scores[0], scores[1]]
	var played := PackedStringArray()
	for pair: Array in state.get("table/cards", []):
		played.append("%d: %s" % [pair[0], pair[1]])
	_played.text = "Table: " + ", ".join(played)
	if state.get("table/over", false):
		var winner: int = state.get("table/winner", 0)
		_status.text = "A draw." if winner == 0 else "Seat %d wins." % winner
	elif not away.is_empty():
		_status.text = "Waiting for seat %d..." % away[0]
	elif turn == seat():
		_status.text = "Your turn (seat %d): play a card." % seat()
	else:
		_status.text = "Seat %d's turn." % turn
	for child in _hand.get_children():
		_hand.remove_child(child)
		child.queue_free()
	var cards := hand()
	for i in cards.size():
		var button := Button.new()
		button.text = "%s (%d)" % [cards[i], CardsGame.value_of(cards[i])]
		button.pressed.connect(play.bind(i))
		_hand.add_child(button)
```

## The dealer: `dealer.gd`

The dealer is where the network meets the rules, on the server only (on a client `start()` returns at
once). It holds the `CardsGame`, answers `play_card`, and writes the result into the keyed state.

```gdscript file=demos/cards/dealer.gd
class_name CardsDealer
extends Node
## The dealer: the one networked node of the demo, under the table's NetManager so clients can call it.
## On the server it holds the rules (a CardsGame), answers `play_card` and writes the table and the
## hands into the keyed state, which every peer's table shows. It replicates nothing itself.

## Where the server saves the game after every card ("" = never).
var save_path: String = "user://cards_save.bin"
## The random source of the deal.
var rng: RandomNumberGenerator = RandomNumberGenerator.new()
## Offline: the house plays seat 2 (its first card, at once).
var house: bool = false
## The rules, on the server once `start()` ran.
var game: CardsGame

var _session: NetSession


func _init() -> void:
	rng.randomize()


## Called once the session runs. The server deals (or goes on with `saved`, the game part of a save
## file) and publishes the table; a client does nothing here.
func start(saved: Dictionary = {}) -> void:
	_session = Net.session_for(self)
	if not _session.is_server():
		return
	_session.rejoin_timeout_s = -1  # a seat waits for its player for the whole game
	for seat in [1, 2]:
		_session.state.view("hand/%d/" % seat, func(peer: int) -> bool: return _session.seat(peer) == seat)
	game = CardsGame.from_dict(saved) if not saved.is_empty() else CardsGame.deal(rng)
	for sig: Signal in [_session.peer_joined, _session.peer_left, _session.peer_dropped, _session.seat_released]:
		sig.connect(_seats_changed.unbind(1))
	_session.peer_rejoined.connect(_seats_changed.unbind(2))
	_publish()
	_publish_seats()
	save()


## Asked by a player's table with `Net.server(dealer.play_card, index)`; runs on the server. Returns ""
## when the card was played, otherwise why not.
func play_card(index: int, info: NetCallInfo) -> String:
	var seat := _session.seat(info.sender)
	if seat != 1 and seat != 2:
		return "you are watching"
	var missing := _missing_seat()
	if missing != 0:
		return "waiting for seat %d" % missing
	var why := game.play(seat, index)
	if why.is_empty():
		_after_play()
	return why


## The first seat of the two that is not at the table (0 when both are).
func _missing_seat() -> int:
	for seat in [1, 2]:
		if not _present(seat):
			return seat
	return 0


func _present(seat: int) -> bool:
	return (house and seat == 2) or _session.peer_of(seat) != 0


func _after_play() -> void:
	if house and game.turn == 2 and not game.is_over():
		game.play(2, 0)
	_publish()
	save()


func _publish() -> void:
	var state := _session.state
	state.set("table/cards", game.table)
	state.set("table/scores", game.scores)
	state.set("table/turn", game.turn)
	state.set("table/winner", game.winner())
	state.set("table/over", game.is_over())
	for seat in [1, 2]:
		state.set("hand/%d/cards" % seat, game.hand(seat))


func _seats_changed() -> void:
	_publish_seats.call_deferred()


## `table/away`: the seats of the two that are not at the table.
func _publish_seats() -> void:
	var away: Array = []
	for seat in [1, 2]:
		if not _present(seat):
			away.append(seat)
	_session.state.set("table/away", away)


## Writes the session state and the game to `save_path`.
func save() -> Error:
	if save_path.is_empty():
		return OK
	var file := FileAccess.open(save_path, FileAccess.WRITE)
	if file == null:
		return FileAccess.get_open_error()
	file.store_var({"session": _session.save_state(), "game": game.to_dict()})
	return OK


## A save file's `{session, game}`; empty when there is none.
static func read_save(path: String) -> Dictionary:
	var file := FileAccess.open(path, FileAccess.READ)
	if file == null:
		return {}
	var d: Variant = file.get_var()
	return d if d is Dictionary else {}
```

**`play_card` validates in the rules.** Any peer may call a method with `Net.server` (guide 06); the
method decides. The trailing `info: NetCallInfo` parameter says who called (`info.sender`, a peer id),
`Net.seat(peer)` turns that into a seat, and the rules say whether that seat may play. This is game
logic, not a security boundary (plan 12.1): the server is the one place the rules run, so every peer
sees the same game.

**The table is keyed state.** `_publish()` writes `table/cards`, `table/scores`, `table/turn`,
`table/winner` and `table/over` after every play. The server is the only writer (keyed paths are
server-only until `allow` opens them), and a peer that joins late receives the whole map at once.

**Each hand is private.** Seat 1's cards are at `hand/1/cards`, seat 2's at `hand/2/cards`, and two
views decide who receives them:

```gdscript
_session.state.view("hand/%d/" % seat, func(peer: int) -> bool: return _session.seat(peer) == seat)
```

A path under a view's prefix reaches only the peers its filter admits, and the server asks the filter
again every tick; the filter names a seat, not a peer id, so after a rejoin the new connection is
admitted and nobody else is. The server itself holds both hands, as it holds everything (views decide
delivery, not secrecy from the server). The test scans every packet each client receives, at every bit
offset, for the name of a card the other seat holds, and finds none.

**Seats and rejoining.** `rejoin_timeout_s = -1` holds a dropped player's seat for the whole game (the
default is 60 s; 0 gives it up at once). While seat 2 is away, `peer_of(2)` is 0: `table/away` lists it,
every table says "Waiting for seat 2...", and `play_card` refuses with the same words, so the turn never
moves past an absent player. When the player joins again with the same key it gets seat 2 back, under a
new peer id; the view admits the new id at the next tick and its hand arrives with the rest of the keyed
state. Someone who joins in the meantime gets seat 3, not the held seat. The seat signals
(`peer_dropped`, `peer_rejoined`, `seat_released`, and `peer_joined` / `peer_left`) only trigger a
redraw of `table/away`.

**Save and resume.** After the deal and after every card the server writes `save_path`: one `store_var`
of a dictionary with `Net.save_state()` (here `session.save_state()`: the keyed state with the table and
both hands, the seats and their keys) and the game's own `to_dict()`. Views are code, not state, so
`start()` declares them again on a resumed game. To go on later, host from the file (`main.gd`): the
saved seats are reserved, the host takes the host's seat, and each player who joins with the key they had
(their Steam id, or the name they typed) sits down on their seat with their hand. The tests point
`save_path` at a file of their own, so test processes running side by side never share one.

## What to look at while it runs

* **The meter** (F9, or `Net.show_meter(true)`) shows the keyed bytes, and `my_seat`, `seats` and
  `seats_away` among `Net.stats()`.
* `Net.seat_table()` lists every seat with its peer and whether it is away; `Net.state.keys("hand/")` on a
  client lists only that player's hand.
