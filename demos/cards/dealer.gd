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
