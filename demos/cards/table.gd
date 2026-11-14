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
