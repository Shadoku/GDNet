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
