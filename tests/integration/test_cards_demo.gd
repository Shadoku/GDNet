extends TestCase
## WP17 (docs/PLAN_NETCODE.md 12.3, gates G2, G8 and G12): the Cards demo (`demos/cards/`) under
## NetFixture, as a host-player (the host is seat 1, one client seat 2) and as a dedicated server (two
## clients): a whole game, a play out of turn, a seat dropped mid-turn under loss and back, hands that
## never reach the other seat, a saved game hosted again, and the README's listings against the files.

const TABLE := preload("res://demos/cards/table.tscn")
const SEED := 3

var net: NetFixture
var _saves: Array[String] = []


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null
	for path in _saves:
		DirAccess.remove_absolute(ProjectSettings.globalize_path(path))
	_saves.clear()


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


## A save file of this test process alone (the integration stage runs test files side by side).
func _save_path() -> String:
	var path := "user://cards_test_%d_%d.bin" % [OS.get_process_id(), Time.get_ticks_usec()]
	_saves.append(path)
	return path


func _rng(seed_value: int) -> RandomNumberGenerator:
	var rng := RandomNumberGenerator.new()
	rng.seed = seed_value
	return rng


## The game the dealer deals with SEED.
func _expected() -> CardsGame:
	return CardsGame.deal(_rng(SEED))


## A started fixture with the seat keys "alice" and "bob": one client as a host-player, two dedicated.
func _fixture(mode: NetFixture.HostMode, state: PackedByteArray = PackedByteArray()) -> NetFixture:
	var f := NetFixture.new(self, 1 if mode == NetFixture.HostMode.HOST_PLAYER else 2, mode)
	var keys := ["alice", "bob"]
	for i in f.client_sessions.size():
		f.client_sessions[i].key = keys[i]
	f.start_server(state)
	return f


## Runs `scenario(f)` once per host mode on a connected fixture whose tables run.
func _both(scenario: Callable) -> void:
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		net = _fixture(mode)
		net.start_clients()
		assert_true(await net.wait_connected(), "connected")
		_setup(net)
		await scenario.call(net)
		net.teardown()
		net = null


func _add_table(branch: Node) -> CardsTable:
	var t: CardsTable = TABLE.instantiate()
	t.name = "Table"
	branch.add_child(t)
	return t


## A table in every branch; the server's dealer deals with SEED (or goes on with `saved`); with
## `start_tables`, every table starts (their sessions must run).
func _setup(f: NetFixture, saved: Dictionary = {}, start_tables: bool = true) -> void:
	for branch: Node in [f.server] + f.clients:
		_add_table(branch)
	var dealer := _table(f.server).dealer
	dealer.rng.seed = SEED
	dealer.save_path = _save_path()
	dealer.start(saved)
	if start_tables:
		for branch: Node in [f.server] + f.clients:
			_table(branch).start()


func _table(branch: Node) -> CardsTable:
	return branch.get_node("Table")


func _branches(f: NetFixture) -> Array:
	return [f.server] + f.clients.filter(func(c: Node) -> bool: return c != null)


## The branch that plays `seat`.
func _player(f: NetFixture, seat: int) -> Node:
	for branch: Node in _branches(f):
		if f.session(branch).my_seat() == seat:
			return branch
	return null


func _client_index(f: NetFixture, seat: int) -> int:
	return f.clients.find(_player(f, seat))


## The public table a branch knows.
func _public(branch: Node, f: NetFixture) -> Dictionary:
	var state := f.session(branch).state
	var out := {}
	for path in state.keys("table/"):
		out[path] = state.get(path)
	return out


func _everyone_has(f: NetFixture, cards_on_table: int) -> bool:
	for branch: Node in _branches(f):
		if (f.session(branch).state.get("table/cards", []) as Array).size() != cards_on_table:
			return false
	return true


func _hands_arrived(f: NetFixture) -> bool:
	for seat in [1, 2]:
		var p := _player(f, seat)
		if p == null or _table(p).hand().size() != CardsGame.HAND_SIZE - _played_by(f, seat):
			return false
	return true


func _played_by(f: NetFixture, seat: int) -> int:
	var n := 0
	for pair: Array in f.server_session.state.get("table/cards", []):
		if int(pair[0]) == seat:
			n += 1
	return n


func test_a_whole_game_reaches_the_same_table_everywhere() -> void:
	await _both(func(f: NetFixture) -> void:
		var expect := _expected()
		assert_true(await f.wait_until(func() -> bool: return _hands_arrived(f)), "each seat holds its hand")
		assert_eq(_table(_player(f, 1)).hand(), expect.hand(1), "the seeded deal")
		for i in CardsGame.ROUNDS * 2:
			var seat := expect.turn
			assert_eq(await _table(_player(f, seat)).play(0), "", "seat %d plays" % seat)
			expect.play(seat, 0)
			assert_true(await f.wait_until(func() -> bool: return _everyone_has(f, i + 1)), "card %d reached everyone" % (i + 1))
		assert_true(await f.wait_until(func() -> bool:
			for b: Node in _branches(f):
				if _public(b, f) != _public(f.server, f):
					return false
			return true), "the same table on every peer")
		var table := _public(f.server, f)
		assert_eq(table["table/cards"], expect.table)
		assert_eq(table["table/scores"], expect.scores)
		assert_eq(table["table/over"], true)
		assert_eq(table["table/winner"], expect.winner())
		var text := "A draw." if expect.winner() == 0 else "Seat %d wins." % expect.winner()
		for b: Node in _branches(f):
			assert_eq((_table(b).get_node("Box/Status") as Label).text, text, "%s shows the result" % b.name)
		assert_eq(_table(_player(f, 2)).get_node("Box/Hand").get_child_count(), CardsGame.HAND_SIZE - CardsGame.ROUNDS, "two cards left in hand"))


func test_a_play_out_of_turn_is_refused_and_changes_nothing() -> void:
	await _both(func(f: NetFixture) -> void:
		assert_true(await f.wait_until(func() -> bool: return _hands_arrived(f)))
		var before := _table(f.server).dealer.game.to_dict()
		var seat2 := _table(_player(f, 2))
		assert_eq(await seat2.play(0), "it is seat 1's turn")
		assert_eq((seat2.get_node("Box/Status") as Label).text, "it is seat 1's turn", "the player is told")
		await _frames(5)
		assert_eq(_table(f.server).dealer.game.to_dict(), before, "the rules changed nothing")
		for b: Node in _branches(f):
			assert_eq(f.session(b).state.get("table/cards"), [], "%s: nothing on the table" % b.name)
		assert_eq(seat2.hand().size(), CardsGame.HAND_SIZE)
		assert_ne(await _table(_player(f, 1)).play(99), "", "no such card"))


## Gate G12 at demo level: seat 2's client drops mid-turn under emulated loss; the game waits (seat 1
## cannot play, and is told why); the client comes back with its key and has its seat, its hand and the
## turn, and plays on. Meanwhile seat 1's peer never receives seat 2's cards.
func test_g12_a_seat_dropped_mid_turn_rejoins_with_its_hand_and_the_turn() -> void:
	await _both(func(f: NetFixture) -> void:
		var expect := _expected()
		assert_true(await f.wait_until(func() -> bool: return _hands_arrived(f)))
		var i := _client_index(f, 2)
		var link := NetEmulation.new()
		link.send_latency_ms = 30.0
		link.receive_latency_ms = 30.0
		link.send_loss = 0.1
		link.receive_loss = 0.1
		link.seed = 17
		f.set_emulation(f.clients[i], link)
		var secret := (expect.hand(2)[CardsGame.HAND_SIZE - 1] as String).to_utf8_buffer()
		var other := _player(f, 1)
		var other_end: NetPipePeer = f.peer(other)
		if other != f.server:
			other_end._debug_watch(secret)
		assert_eq(await _table(other).play(0), "")
		expect.play(1, 0)
		assert_true(await f.wait_until(func() -> bool: return _everyone_has(f, 1)))
		var hand_before := _table(f.clients[i]).hand()
		assert_eq(hand_before, expect.hand(2))
		f.drop_client(i)
		assert_true(await f.wait_until(func() -> bool: return f.session(other).state.get("table/away") == [2]), "seat 2 is away")
		assert_eq((_table(other).get_node("Box/Status") as Label).text, "Waiting for seat 2...")
		assert_eq(await _table(other).play(0), "waiting for seat 2", "the game waits for the away seat")
		assert_eq(f.server_session.state.get("table/turn"), 2, "still seat 2's turn")
		# Someone else joins meanwhile: the held seat is not theirs.
		var watcher := Node.new()
		watcher.name = "Watcher"
		get_tree().root.add_child(watcher)
		var ws := NetSession.new()
		watcher.add_child(ws)
		ws.start(NetPipePeer.create_client(f.peer(f.server)), watcher.get_path())
		assert_true(await f.wait_until(func() -> bool: return ws.my_seat() > 0), "the watcher sits down")
		assert_eq(ws.my_seat(), 3, "on a new seat: seat 2 is held for its player")
		var t0 := Engine.get_physics_frames()
		var back := f.rejoin_client(i)
		f.set_emulation(back, link)
		var t := _add_table(back)
		t.start()
		assert_true(await f.wait_until(func() -> bool:
			return f.session(back).my_seat() == 2 and t.hand() == hand_before and f.session(back).state.get("table/turn") == 2, 10.0), "back on seat 2 with its hand and the turn")
		print("seat 2 back with its hand %d ticks after reconnecting (30 ms each way, 10 %% loss; mode %d)" % [Engine.get_physics_frames() - t0, f.host_mode])
		assert_true(await f.wait_until(func() -> bool: return f.session(other).state.get("table/away") == []), "the table is full again")
		assert_eq(t.hand(), hand_before, "nothing it had was lost")
		assert_eq(await t.play(0), "", "and it plays on")
		expect.play(2, 0)
		assert_true(await f.wait_until(func() -> bool: return _everyone_has(f, 2), 10.0))
		assert_eq(f.server_session.state.get("table/cards"), expect.table)
		watcher.free()
		if other != f.server:
			assert_gt(other_end._debug_watch_scanned(), 0, "seat 1's peer received packets meanwhile")
			assert_eq(other_end._debug_watch_hits(), 0, "and none held a card of seat 2's hand"))


## No byte of a hand reaches another peer: the pipe end of each peer is scanned at every bit offset for
## a card the other seat holds and never plays (and, as a check that the scan works, for one of its own).
func test_the_hands_never_reach_the_other_seat() -> void:
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		net = _fixture(mode)
		net.start_clients()
		assert_true(await net.wait_connected())
		var expect := _expected()
		var last := CardsGame.HAND_SIZE - 1
		var card1 := (expect.hand(1)[last] as String).to_utf8_buffer()
		var card2 := (expect.hand(2)[last] as String).to_utf8_buffer()
		# Seat 2's peer is the last client in both modes; seat 1's is the host or the first client.
		var end2 := net.peer(net.clients[net.clients.size() - 1])
		var end1: NetPipePeer = null if mode == NetFixture.HostMode.HOST_PLAYER else net.peer(net.clients[0])
		end2._debug_watch(card1)
		if end1 != null:
			end1._debug_watch(card1)
		_setup(net)
		assert_true(await net.wait_until(func() -> bool: return _hands_arrived(net)))
		assert_eq(_player(net, 2), net.clients[net.clients.size() - 1])
		assert_eq(await _table(_player(net, 1)).play(0), "")
		assert_true(await net.wait_until(func() -> bool: return _everyone_has(net, 1)))
		await _frames(5)
		if end1 != null:
			assert_gt(end1._debug_watch_hits(), 0, "the scan finds seat 1's card in seat 1's packets")
		assert_gt(end2._debug_watch_scanned(), 0)
		assert_eq(end2._debug_watch_hits(), 0, "seat 2 never received a card of seat 1's hand (mode %d)" % mode)
		end2._debug_watch(card2)
		if end1 != null:
			end1._debug_watch(card2)
		for i in CardsGame.ROUNDS * 2 - 1:
			var seat := 2 if i % 2 == 0 else 1
			assert_eq(await _table(_player(net, seat)).play(0), "")
			assert_true(await net.wait_until(func() -> bool: return _everyone_has(net, i + 2)))
		await _frames(5)
		assert_gt(end2._debug_watch_hits(), 0, "the scan finds seat 2's card in seat 2's packets")
		if end1 != null:
			assert_gt(end1._debug_watch_scanned(), 0)
			assert_eq(end1._debug_watch_hits(), 0, "seat 1 never received a card of seat 2's hand")
		net.teardown()
		net = null


## The server saves after every card; hosting from the file gives the same table, hands and seats, and
## each client that joins with its old key (in another order) sits down on its seat with its hand.
func test_a_saved_game_hosted_again_has_its_table_hands_and_seats() -> void:
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		net = _fixture(mode)
		net.start_clients()
		assert_true(await net.wait_connected())
		_setup(net)
		assert_true(await net.wait_until(func() -> bool: return _hands_arrived(net)))
		for seat in [1, 2, 1]:
			assert_eq(await _table(_player(net, seat)).play(0), "")
		var path := _table(net.server).dealer.save_path
		var seats := {}
		var hands := {}
		for c: Node in net.clients:
			seats[net.session(c).key] = net.session(c).my_seat()
		for seat in [1, 2]:
			hands[seat] = _table(net.server).dealer.game.hand(seat).duplicate()
		var table := _public(net.server, net)
		net.teardown()
		net = null
		await _frames(2)
		var saved := CardsDealer.read_save(path)
		assert_false(saved.is_empty(), "the server wrote the save file")
		print("save file %d bytes, of which the session state %d (mode %d)" % [FileAccess.get_file_as_bytes(path).size(), (saved["session"] as PackedByteArray).size(), mode])
		net = NetFixture.new(self, seats.size(), mode)
		var keys: Array = seats.keys()
		keys.reverse()
		for i in keys.size():
			net.client_sessions[i].key = keys[i]
		net.start_server(saved["session"])
		_setup(net, saved["game"], false)
		net.start_clients()
		assert_true(await net.wait_connected())
		for branch: Node in [net.server] + net.clients:
			_table(branch).start()
		for c: Node in net.clients:
			assert_eq(net.session(c).my_seat(), seats[net.session(c).key], "%s sits on its old seat" % net.session(c).key)
		assert_true(await net.wait_until(func() -> bool:
			return _hands_arrived(net) and _public(_player(net, 2), net) == table), "the table and the hands came back")
		for seat in [1, 2]:
			assert_eq(_table(_player(net, seat)).hand(), hands[seat], "seat %d's hand" % seat)
		assert_eq(net.server_session.state.get("table/turn"), 2)
		assert_eq(await _table(_player(net, 2)).play(0), "", "the game goes on")
		net.teardown()
		net = null


func test_g8_the_readme_listings_are_the_files() -> void:
	var readme := FileAccess.get_file_as_string("res://demos/cards/README.md")
	assert_false(readme.is_empty(), "the README exists")
	var listed := {}
	var lines := readme.split("\n")
	var i := 0
	while i < lines.size():
		var line := lines[i]
		if line.begins_with("```") and line.contains(" file="):
			var path := line.get_slice("file=", 1).strip_edges()
			var body := PackedStringArray()
			i += 1
			while i < lines.size() and not lines[i].begins_with("```"):
				body.append(lines[i])
				i += 1
			var on_disk := FileAccess.get_file_as_string("res://" + path).strip_edges(false, true)
			assert_eq("\n".join(body), on_disk, "the README's listing of %s" % path)
			listed[path] = true
		i += 1
	for f in DirAccess.get_files_at("res://demos/cards"):
		if f.ends_with(".gd") or f.ends_with(".tscn"):
			assert_true(listed.has("demos/cards/" + f), "the README lists %s" % f)
