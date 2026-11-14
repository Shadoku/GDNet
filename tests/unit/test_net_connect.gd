extends TestCase
## WP23 (docs/PLAN_NETCODE.md 12.10): the connect layer's pure parts (join codes, address-or-code, the
## settings), the path without GodotSteam, and the Steam flow against FakeSteamBackend. No session is
## started here: `start_peer` records the peer NetConnect would hand to gdnet. Time is the deltas passed
## to `advance()`, never the real clock.

var _made: Array[Node] = []


func after_each() -> void:
	for n in _made:
		if is_instance_valid(n):
			n.free()
	_made.clear()


## A NetConnect on `backend` that records the peers it starts and the signals it emits.
func _connect(backend: NetSteamBackend, seed_value: int = 7) -> NetConnect:
	var c := NetConnect.new()
	_made.append(c)
	c.backend = backend
	c.rng.seed = seed_value
	c.clock = func() -> float: return 1000.0
	var log: Dictionary = {"hosted": [], "joined": 0, "failed": [], "started": []}
	c.set_meta("log", log)
	c.start_peer = func(peer: MultiplayerPeer) -> Error:
		log["started"].append(peer)
		return OK
	c.hosted.connect(func(code: String) -> void: log["hosted"].append(code))
	c.joined.connect(func() -> void: log["joined"] += 1)
	c.failed.connect(func(reason: String) -> void: log["failed"].append(reason))
	return c


func _log(c: NetConnect) -> Dictionary:
	return c.get_meta("log")


## Advances every connector by `seconds` in steps of 0.1 s.
func _run(connectors: Array, seconds: float = 0.5) -> void:
	var t := 0.0
	while t < seconds - 0.0001:
		for c: NetConnect in connectors:
			c.advance(0.1)
		t += 0.1


## A host on Steam with its code, and a client sharing its world.
func _host_and_client() -> Array:
	var world := {}
	var host := _connect(FakeSteamBackend.new(world, 1001), 7)
	var client := _connect(FakeSteamBackend.new(world, 2002), 8)
	assert_eq(host.host_steam(), OK)
	_run([host])
	return [host, client, world]


func test_codes_use_six_characters_without_look_alikes() -> void:
	for ch in ["0", "O", "1", "I", "L"]:
		assert_false(NetConnect.CODE_ALPHABET.contains(ch), "no %s in the alphabet" % ch)
	assert_eq(NetConnect.CODE_ALPHABET.length(), 31)
	var rng := RandomNumberGenerator.new()
	rng.seed = 42
	var a := NetConnect.make_code(rng)
	assert_eq(a.length(), 6)
	for ch in a:
		assert_true(NetConnect.CODE_ALPHABET.contains(ch), "%s from the alphabet" % ch)
	rng.seed = 42
	assert_eq(NetConnect.make_code(rng), a, "the same seed gives the same code")
	var seen := {}
	for i in 50:
		seen[NetConnect.make_code(rng)] = true
	assert_gt(seen.size(), 45, "codes vary")


func test_a_join_field_holds_an_address_or_a_code() -> void:
	assert_true(NetConnect.is_join_code("K7P2QX"))
	assert_true(NetConnect.is_join_code(" k7p-2qx "), "case, spaces and dashes do not matter")
	assert_eq(NetConnect.normalise_code(" k7p-2qx "), "K7P2QX")
	assert_false(NetConnect.is_join_code("127.0.0.1"))
	assert_false(NetConnect.is_join_code("localhost"))
	assert_false(NetConnect.is_join_code("K7P2Q0"), "0 is not in the alphabet")
	assert_false(NetConnect.is_join_code("K7P2Q"), "too short")
	assert_false(NetConnect.is_join_code(""))
	assert_eq(NetConnect.parse_address("192.168.1.20", 7350), {"address": "192.168.1.20", "port": 7350})
	assert_eq(NetConnect.parse_address(" box.lan:9000 ", 7350), {"address": "box.lan", "port": 9000})
	assert_eq(NetConnect.parse_address("", 7350), {"address": "127.0.0.1", "port": 7350})
	assert_eq(NetConnect.parse_address("::1", 7350), {"address": "::1", "port": 7350}, "an IPv6 address keeps its colons")


func test_settings_give_the_app_id_and_the_game_name() -> void:
	var c := NetConnect.new()
	_made.append(c)
	assert_eq(c.app_id, 480, "Spacewar unless gdnet/connect/steam_app_id says otherwise")
	assert_eq(c.game_name, String(ProjectSettings.get_setting("gdnet/connect/game_name")))
	assert_false(c.game_name.is_empty())


func test_without_godotsteam_there_is_no_steam_and_hosting_says_why() -> void:
	var real := NetSteamBackend.new()
	real.singleton_name = "NoSuchSteamSingleton"
	var c := _connect(real)
	assert_eq(c.available(), PackedStringArray(["enet", "offline"]))
	assert_true(c.steam_reason().contains("GodotSteam"), c.steam_reason())
	assert_eq(c.host_steam(), ERR_UNAVAILABLE)
	assert_eq(c.join_steam("K7P2QX"), ERR_UNAVAILABLE)
	assert_eq(_log(c)["failed"].size(), 2)
	assert_true(String(_log(c)["failed"][0]).contains("GodotSteam"), _log(c)["failed"][0])
	assert_eq(_log(c)["started"].size(), 0)


func test_a_fake_steam_is_available_with_the_app_id() -> void:
	var fake := FakeSteamBackend.new()
	var c := _connect(fake)
	assert_eq(c.available(), PackedStringArray(["enet", "steam", "offline"]))
	assert_eq(fake.started_with, 480)
	fake = FakeSteamBackend.new()
	fake.start_reason = "the Steam client is not running"
	c = _connect(fake)
	assert_false(c.available().has("steam"))
	assert_eq(c.steam_reason(), "the Steam client is not running")


func test_hosting_makes_a_lobby_with_code_and_game_name() -> void:
	var fake := FakeSteamBackend.new({}, 1001)
	var c := _connect(fake, 7)
	assert_eq(c.host_steam(), OK)
	assert_eq(_log(c)["hosted"].size(), 0, "nothing before Steam answers")
	_run([c])
	assert_eq(_log(c)["hosted"].size(), 1)
	var code: String = _log(c)["hosted"][0]
	var rng := RandomNumberGenerator.new()
	rng.seed = 7
	assert_eq(code, NetConnect.make_code(rng), "the code comes from the injected random source")
	assert_eq(c.code(), code)
	var lobby: int = fake.world["lobbies"].keys()[0]
	assert_eq(fake.lobby_data(lobby, NetConnect.KEY_CODE), code)
	assert_eq(fake.lobby_data(lobby, NetConnect.KEY_GAME), c.game_name)
	assert_eq(fake.lobby_data(lobby, NetConnect.KEY_HOST), "1001")
	assert_eq(fake.lobby_data(lobby, NetConnect.KEY_CREATED), "1000")
	assert_eq(_log(c)["started"], [fake.peers[0]], "the Steam host peer went to gdnet")
	assert_eq(fake.world["lobbies"][lobby]["max"], c.max_players)


func test_a_client_resolves_the_code_and_connects_to_the_host() -> void:
	var hc := _host_and_client()
	var host: NetConnect = hc[0]
	var client: NetConnect = hc[1]
	var code: String = _log(host)["hosted"][0]
	assert_eq(client.join_steam(code.to_lower()), OK)
	_run([host, client])
	assert_eq(_log(client)["failed"], [])
	assert_eq(_log(client)["joined"], 1)
	var fake: FakeSteamBackend = client.backend
	assert_eq(fake.peers.size(), 1)
	assert_eq(_log(client)["started"], [fake.peers[0]])
	assert_eq((fake.peers[0] as NetPipePeer).get_unique_id() > 1, true, "a client end")


func test_an_unknown_code_fails() -> void:
	var hc := _host_and_client()
	var client: NetConnect = hc[1]
	var other := "ZZZZZZ" if _log(hc[0])["hosted"][0] != "ZZZZZZ" else "YYYYYY"
	client.join_steam(other)
	_run([client])
	assert_eq(_log(client)["failed"].size(), 1)
	assert_true(String(_log(client)["failed"][0]).contains("no game"), _log(client)["failed"][0])
	assert_eq(_log(client)["started"].size(), 0)


func test_a_lobby_of_another_game_with_the_same_code_is_not_joined() -> void:
	var world := {}
	var stranger := FakeSteamBackend.new(world, 3003)
	stranger.add_lobby(3003, {NetConnect.KEY_CODE: "K7P2QX", NetConnect.KEY_GAME: "someone else's game", NetConnect.KEY_HOST: "3003"})
	var client := _connect(FakeSteamBackend.new(world, 2002))
	client.join_steam("K7P2QX")
	_run([client])
	assert_eq(_log(client)["failed"].size(), 1, "app 480 is shared: the game name filters")
	assert_eq(_log(client)["joined"], 0)


func test_two_lobbies_with_one_code_take_the_newest() -> void:
	var world := {}
	var a := FakeSteamBackend.new(world, 3003)
	var b := FakeSteamBackend.new(world, 4004)
	var client_fake := FakeSteamBackend.new(world, 2002)
	var c := _connect(client_fake)
	var data := {NetConnect.KEY_CODE: "K7P2QX", NetConnect.KEY_GAME: c.game_name}
	data[NetConnect.KEY_HOST] = "3003"
	data[NetConnect.KEY_CREATED] = "900"
	a.add_lobby(3003, data)
	a.host_peer()
	data[NetConnect.KEY_HOST] = "4004"
	data[NetConnect.KEY_CREATED] = "950"
	var newest := b.add_lobby(4004, data)
	b.host_peer()
	c.join_steam("K7P2QX")
	_run([c])
	assert_eq(_log(c)["joined"], 1)
	assert_has(world["lobbies"][newest]["members"], 2002, "joined the newer lobby")
	var server: NetPipePeer = world["servers"][4004]
	assert_eq((client_fake.peers[0] as NetPipePeer).get_unique_id() > 1, true)
	assert_ne(server, null)


func test_a_full_game_refuses_the_client() -> void:
	var world := {}
	var host := _connect(FakeSteamBackend.new(world, 1001))
	host.max_players = 1
	host.host_steam()
	_run([host])
	var client := _connect(FakeSteamBackend.new(world, 2002))
	client.join_steam(host.code())
	_run([client])
	assert_eq(_log(client)["failed"].size(), 1)
	assert_true(String(_log(client)["failed"][0]).contains("full"), _log(client)["failed"][0])
	assert_eq(_log(client)["started"].size(), 0)


func test_a_refused_join_fails_with_steams_reason() -> void:
	var hc := _host_and_client()
	var client: NetConnect = hc[1]
	(client.backend as FakeSteamBackend).join_error = "you are banned from this game"
	client.join_steam(hc[0].code())
	_run([client])
	assert_eq(_log(client)["failed"], ["you are banned from this game"])


func test_a_closed_game_is_not_found_and_leave_closes_the_lobby() -> void:
	var hc := _host_and_client()
	var host: NetConnect = hc[0]
	var client: NetConnect = hc[1]
	var code := host.code()
	host.leave()
	assert_eq(hc[2]["lobbies"].size(), 0, "the host's lobby is gone")
	assert_eq(host.code(), "")
	client.join_steam(code)
	_run([client])
	assert_eq(_log(client)["failed"].size(), 1)
	assert_true(String(_log(client)["failed"][0]).contains("no game"))


func test_a_lobby_whose_host_left_is_refused() -> void:
	var world := {}
	var stale := FakeSteamBackend.new(world, 3003)
	var c := _connect(FakeSteamBackend.new(world, 2002))
	# Steam passed the lobby to another member when its host (5005) left; the data still names 5005.
	stale.add_lobby(3003, {NetConnect.KEY_CODE: "K7P2QX", NetConnect.KEY_GAME: c.game_name, NetConnect.KEY_HOST: "5005", NetConnect.KEY_CREATED: "1"})
	c.join_steam("K7P2QX")
	_run([c])
	assert_eq(_log(c)["failed"].size(), 1)
	assert_true(String(_log(c)["failed"][0]).contains("host has left"), _log(c)["failed"][0])
	assert_eq(_log(c)["started"].size(), 0)


func test_the_host_leaving_ends_the_clients_game() -> void:
	var hc := _host_and_client()
	var host: NetConnect = hc[0]
	var client: NetConnect = hc[1]
	client.join_steam(host.code())
	_run([host, client])
	assert_eq(_log(client)["joined"], 1)
	host.leave()
	_run([client])
	assert_eq(_log(client)["failed"].size(), 1)
	assert_true(String(_log(client)["failed"][0]).contains("host left"), _log(client)["failed"][0])
	assert_eq(hc[2]["lobbies"].size(), 0, "the client left the lobby too, so it closed")


func test_steam_not_answering_times_out_on_injected_time() -> void:
	var fake := FakeSteamBackend.new()
	fake.silent = true
	var c := _connect(fake)
	c.timeout_s = 10.0
	c.join_steam("K7P2QX")
	_run([c], 9.5)
	assert_eq(_log(c)["failed"].size(), 0, "not before the timeout")
	_run([c], 1.0)
	assert_eq(_log(c)["failed"].size(), 1)
	assert_true(String(_log(c)["failed"][0]).contains("did not answer"), _log(c)["failed"][0])
	c.host_steam()
	_run([c], 10.5)
	assert_eq(_log(c)["failed"].size(), 2, "hosting times out too")


func test_failing_to_create_a_lobby_or_a_peer_fails() -> void:
	var fake := FakeSteamBackend.new()
	fake.create_error = "Steam could not create a lobby (result 2)"
	var c := _connect(fake)
	c.host_steam()
	_run([c])
	assert_eq(_log(c)["failed"], ["Steam could not create a lobby (result 2)"])
	fake.create_error = ""
	fake.peer_error = "no relay"
	c.host_steam()
	_run([c])
	assert_eq(_log(c)["failed"].size(), 2)
	assert_true(String(_log(c)["failed"][1]).contains("no relay"))
	assert_eq(fake.world["lobbies"].size(), 0, "the lobby is closed again")


func test_a_friends_list_join_request_joins_that_lobby() -> void:
	var hc := _host_and_client()
	var host: NetConnect = hc[0]
	var client: NetConnect = hc[1]
	var lobby: int = hc[2]["lobbies"].keys()[0]
	(client.backend as FakeSteamBackend).request_join(lobby)
	_run([host, client])
	assert_eq(_log(client)["joined"], 1)
	assert_eq(_log(client)["failed"], [])
	assert_eq(host.code().is_empty(), false)


## WP17: over Steam the seat key is the player's Steam id, so a player who restarts the game takes their
## seat back; ENet and offline leave the session's key alone, and a key the game sets wins everywhere.
func test_a_steam_session_joins_with_the_steam_id_as_its_seat_key() -> void:
	var hc := _host_and_client()
	var host: NetConnect = hc[0]
	var client: NetConnect = hc[1]
	assert_eq(host.session_key(), "steam:1001", "the host's key too")
	client.join_steam(host.code())
	_run([host, client])
	assert_eq(_log(client)["joined"], 1)
	assert_eq(client.session_key(), "steam:2002")
	var enet := _connect(FakeSteamBackend.new({}, 3003))
	assert_eq(enet.join_enet("127.0.0.1", 7350), OK)
	assert_eq(enet.session_key(), "", "ENet keeps gdnet's own key")
	enet.offline()
	assert_eq(enet.session_key(), "")
	enet.key = "slot-3"
	enet.join_enet("127.0.0.1", 7350)
	assert_eq(enet.session_key(), "slot-3", "a key the game sets is used on every transport")
	enet.leave()


## WP17 decision (1): a join refused because the key's seat is still connected (the server has not yet
## noticed the old connection drop) is tried again, `retry_delay_s` apart on the injected time, at most
## `join_retries` times; any other refusal is not retried.
func test_a_join_refused_because_the_seat_is_still_held_is_retried() -> void:
	var c := _connect(FakeSteamBackend.new({}, 3003))
	c.retry_delay_s = 2.0
	c.join_retries = 2
	var held := "refused: this seat key is already in use: seat 2 is held by a connected peer (5); a seat takes one connection at a time"
	assert_eq(c.join_enet("127.0.0.1", 7350), OK)
	assert_eq(_log(c)["started"].size(), 1)
	c.session_ended(held)
	_run([c], 1.5)
	assert_eq(_log(c)["started"].size(), 1, "not before the delay")
	_run([c], 1.0)
	assert_eq(_log(c)["started"].size(), 2, "tried again")
	assert_eq(_log(c)["failed"], [])
	c.session_ended(held)
	_run([c], 2.5)
	assert_eq(_log(c)["started"].size(), 3, "a second time")
	c.session_ended(held)
	_run([c], 2.5)
	assert_eq(_log(c)["started"].size(), 3, "no third retry")
	assert_eq(_log(c)["failed"].size(), 1, "it gives up and says why")
	assert_true(String(_log(c)["failed"][0]).contains("already in use"), _log(c)["failed"][0])
	# A new join starts counting again; another refusal is not retried.
	c.join_enet("127.0.0.1", 7350)
	c.session_ended("protocol mismatch: the wire format differs")
	_run([c], 5.0)
	assert_eq(_log(c)["started"].size(), 4, "a mismatch is not retried")
	c.leave()
