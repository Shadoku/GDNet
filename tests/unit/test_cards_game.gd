extends TestCase
## WP17 (docs/PLAN_NETCODE.md 12.3): the Cards demo's rules, `demos/cards/cards_game.gd`, a plain
## RefCounted with no node and no network: dealing from a seeded deck, turns, refusals, scoring, the end
## of the game and the state the demo saves.


func _rng(seed_value: int) -> RandomNumberGenerator:
	var rng := RandomNumberGenerator.new()
	rng.seed = seed_value
	return rng


func test_a_deal_gives_each_seat_five_different_cards_from_the_seed() -> void:
	var g := CardsGame.deal(_rng(3))
	assert_eq(g.hand(1).size(), CardsGame.HAND_SIZE)
	assert_eq(g.hand(2).size(), CardsGame.HAND_SIZE)
	var seen := {}
	for card: String in g.hand(1) + g.hand(2):
		assert_true(CardsGame.DECK.has(card), card)
		seen[card] = true
	assert_eq(seen.size(), CardsGame.HAND_SIZE * 2, "no card twice")
	assert_eq(CardsGame.deal(_rng(3)).hand(1), g.hand(1), "the same seed deals the same hands")
	assert_ne(CardsGame.deal(_rng(4)).hand(1), g.hand(1), "another seed deals others")
	assert_eq(g.turn, 1, "seat 1 starts")
	assert_eq(g.table, [])
	assert_eq(g.scores, [0, 0])


func test_seats_alternate_and_a_play_out_of_turn_changes_nothing() -> void:
	var g := CardsGame.deal(_rng(3))
	var before := g.to_dict()
	assert_eq(g.play(2, 0), "it is seat 1's turn")
	assert_eq(g.to_dict(), before, "refused: nothing changed")
	assert_ne(g.play(1, 9), "", "no such card")
	assert_ne(g.play(3, 0), "", "seat 3 does not play")
	assert_eq(g.to_dict(), before)
	var card: String = g.hand(1)[2]
	assert_eq(g.play(1, 2), "")
	assert_eq(g.table, [[1, card]])
	assert_eq(g.scores, [CardsGame.value_of(card), 0])
	assert_false(g.hand(1).has(card), "it left the hand")
	assert_eq(g.turn, 2)
	assert_eq(g.play(1, 0), "it is seat 2's turn")


func test_the_higher_total_after_the_rounds_wins() -> void:
	var g := CardsGame.deal(_rng(11))
	var totals := [0, 0]
	for i in CardsGame.ROUNDS * 2:
		assert_false(g.is_over())
		var seat := g.turn
		totals[seat - 1] += CardsGame.value_of(g.hand(seat)[0])
		assert_eq(g.play(seat, 0), "")
	assert_true(g.is_over())
	assert_eq(g.scores, totals)
	var expected := 0 if totals[0] == totals[1] else (1 if totals[0] > totals[1] else 2)
	assert_eq(g.winner(), expected)
	assert_eq(g.hand(1).size(), CardsGame.HAND_SIZE - CardsGame.ROUNDS, "the unplayed cards stay in hand")
	assert_eq(g.play(g.turn, 0), "the game is over")
	assert_eq(CardsGame.value_of("Ash Wren"), 1)
	assert_eq(CardsGame.value_of("Prism Wyrm"), 16)


func test_the_saved_dictionary_restores_the_game() -> void:
	var g := CardsGame.deal(_rng(5))
	g.play(1, 1)
	g.play(2, 3)
	var copy := CardsGame.from_dict(g.to_dict())
	assert_eq(copy.to_dict(), g.to_dict())
	assert_eq(copy.turn, 1)
	assert_eq(copy.play(1, 0), "")
	assert_eq(g.hand(1).size(), CardsGame.HAND_SIZE - 1, "the copy shares nothing with the original")
