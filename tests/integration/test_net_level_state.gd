extends TestCase
## WP16 level state (docs/PLAN_NETCODE.md 12.2 "Level state", decision D20): a level the server unloads
## keeps its entities' state for the session and gets it back at the next load; `keep_state` (manager
## and project setting) and `reset_level` give a fresh level; `level_state` and the `state` argument of
## `load_level` hand the same bytes to the game. Fixtures: tests/fixtures/levels/ (a vault with a chest
## holding a gem, and a coin); the lobby is tests/fixtures/rooms/interior_b.tscn.

const VAULT := "res://tests/fixtures/levels/vault.tscn"
const LOBBY := "res://tests/fixtures/rooms/interior_b.tscn"
const COIN := preload("res://tests/fixtures/levels/coin.tscn")
const CHEST := preload("res://tests/fixtures/levels/chest.tscn")
const PLAYER := preload("res://tests/fixtures/net/predicted_player.tscn")
const KEEP := "gdnet/levels/keep_state"
const SPACING := "gdnet/levels/spacing"

var net: NetFixture
## project.godot's `gdnet/levels/spacing` (4000, for the Doors demo), put back after each test.
var _project_spacing: Variant = null
var offline_world: Node2D


func before_each() -> void:
	# These tests choose placement themselves: off unless a test sets it.
	_project_spacing = ProjectSettings.get_setting(SPACING, null)
	ProjectSettings.set_setting(SPACING, null)


func after_each() -> void:
	ProjectSettings.set_setting(KEEP, null)
	ProjectSettings.set_setting(SPACING, _project_spacing)
	Net.empty_level_timeout = 0.0
	if net != null:
		net.teardown()
		net = null
	if Net.is_online():
		for p in Net.levels():
			Net.unload_level(p)
		Net.leave()


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


## Plays the server's vault: opens the chest (a cold property), takes the coin and the gem in the chest
## (level content destroyed), drops a coin (a run-time spawn), puts a coin in the chest (spawned under
## level content) and drops a bag with a coin in it (spawned under a spawned entity).
func _play(level: Node) -> void:
	var m := level.get_node("NetManager")
	var chest := m.get_node("Chest")
	chest.open = true
	m.get_node("Coin").queue_free()
	m.get_node("Chest/Items/Gem").queue_free()
	var dropped: Node2D = COIN.instantiate()
	dropped.name = "Dropped"
	dropped.value = 7
	dropped.position = Vector2(5, 6)
	m.add_child(dropped)
	var inside: Node2D = COIN.instantiate()
	inside.name = "Inside"
	inside.value = 9
	chest.get_node("Items").add_child(inside)
	var bag: Node2D = CHEST.instantiate()
	bag.name = "Bag"
	bag.open = true
	bag.position = Vector2(-30, 40)
	m.add_child(bag)
	var bag_coin: Node2D = COIN.instantiate()
	bag_coin.name = "BagCoin"
	bag_coin.value = 3
	bag.get_node("Items").add_child(bag_coin)


## Every node `_play` touched is spawned (has a net id) on the server.
func _played_and_spawned(level: Node) -> bool:
	var m := level.get_node("NetManager")
	for path in ["Chest", "Dropped", "Chest/Items/Inside", "Bag", "Bag/Items/BagCoin"]:
		var n := m.get_node_or_null(path)
		if n == null or Net.net_id(n) == 0:
			return false
	return not m.has_node("Coin") and not m.has_node("Chest/Items/Gem")


## "" when `level` shows what `_play` did (with `offset` added to positions), else what differs.
func _kept(level: Node, offset: Vector2 = Vector2.ZERO) -> String:
	if level == null:
		return "no level"
	var m := level.get_node_or_null("NetManager")
	if m == null:
		return "no manager"
	var chest := m.get_node_or_null("Chest")
	if chest == null or not chest.open:
		return "the chest is not open"
	if m.has_node("Coin"):
		return "the coin is back"
	if m.has_node("Chest/Items/Gem"):
		return "the gem is back"
	var dropped := m.get_node_or_null("Dropped")
	if dropped == null or dropped.value != 7:
		return "the dropped coin is missing or has another value"
	if not dropped.position.is_equal_approx(Vector2(5, 6) + offset):
		return "the dropped coin is at %s" % dropped.position
	var inside := m.get_node_or_null("Chest/Items/Inside")
	if inside == null or inside.value != 9:
		return "the coin in the chest is missing"
	var bag := m.get_node_or_null("Bag")
	if bag == null or not bag.open:
		return "the bag is missing or closed"
	var bag_coin := m.get_node_or_null("Bag/Items/BagCoin")
	if bag_coin == null or bag_coin.value != 3:
		return "the coin in the bag is missing"
	return ""


## "" when `level` is as authored, else what differs.
func _fresh(level: Node) -> String:
	var m := level.get_node("NetManager")
	if m.get_node("Chest").open:
		return "the chest is open"
	if not m.has_node("Coin") or not m.has_node("Chest/Items/Gem"):
		return "the coin or the gem is missing"
	for path in ["Dropped", "Chest/Items/Inside", "Bag"]:
		if m.has_node(path):
			return "%s is there" % path
	return ""


## A dedicated server with one client, connected; loads the vault and plays it.
func _played_server() -> Node:
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	net.start()
	assert_true(await net.wait_connected())
	net.server_session.empty_level_timeout = 1000.0
	var vault := net.server_session.load_level(VAULT)
	_play(vault)
	assert_true(await net.wait_until(func() -> bool: return _played_and_spawned(vault)), "the play was announced")
	return vault


func test_a_level_reloaded_after_its_timeout_keeps_its_state() -> void:
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		var s := f.server_session
		s.empty_level_timeout = 1000.0
		var lobby := s.load_level(LOBBY)
		var players := {}
		for id in f.players():
			players[id] = Net.spawn_for(id, PLAYER, lobby.get_node("NetManager"), "P%d" % id)
		var vault := s.load_level(VAULT)
		_play(vault)
		assert_true(await f.wait_until(func() -> bool: return _played_and_spawned(vault)), "the play was announced")
		assert_eq(s.level_state(VAULT).size() > 0, true, "a loaded level's current state")
		s.empty_level_timeout = 0.2
		assert_true(await f.wait_until(func() -> bool: return not Array(s.levels()).has(VAULT)), "the empty vault unloads")
		s.empty_level_timeout = 1000.0
		assert_has(Array(s.kept_levels()), VAULT, "its state is kept")
		var kept := s.level_state(VAULT)
		assert_gt(kept.size(), 6, "level_state gives the kept blob")
		print("vault blob: %d bytes" % kept.size())
		s.travel(players[a], VAULT, "Entrance")
		assert_true(await f.wait_until(func() -> bool:
			return ca.has_node("vault/NetManager/Chest") and Net.net_id(ca.get_node("vault/NetManager/Chest")) != 0), "the client entered the vault")
		var sv := f.server.get_node("vault")
		assert_eq(_kept(sv), "", "the server's reloaded vault")
		assert_eq(sv.get_node("NetManager/Chest").at_spawn, {"open": true, "value": 1}, "restored before _net_spawned on the server")
		assert_false(Array(s.kept_levels()).has(VAULT), "the kept state was used")
		assert_true(await f.wait_until(func() -> bool: return _kept(ca.get_node("vault")) == ""), "the client sees the kept state: %s" % _kept(ca.get_node("vault")))
		assert_eq(ca.get_node("vault/NetManager/Chest").at_spawn, {"open": true, "value": 1}, "its Create carried the kept state")
		assert_eq(ca.get_node("vault/NetManager/Bag").at_spawn, {"open": true, "value": 1})
	)
	assert_true(ok, "fixtures connected")


func test_a_client_entering_later_does_not_see_level_content_the_server_destroyed() -> void:
	ProjectSettings.set_setting(KEEP, false)
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		var s := f.server_session
		s.empty_level_timeout = 1000.0
		var lobby := s.load_level(LOBBY)
		var players := {}
		for id in f.players():
			players[id] = Net.spawn_for(id, PLAYER, lobby.get_node("NetManager"), "P%d" % id)
		var vault := s.load_level(VAULT)
		assert_true(await f.wait_until(func() -> bool: return Net.net_id(vault.get_node("NetManager/Coin")) != 0))
		vault.get_node("NetManager/Coin").queue_free()
		await _frames(2)
		s.travel(players[a], VAULT, "Entrance")
		assert_true(await f.wait_until(func() -> bool:
			return ca.has_node("vault/NetManager/Chest") and Net.net_id(ca.get_node("vault/NetManager/Chest")) != 0), "the client entered the vault")
		assert_false(ca.has_node("vault/NetManager/Coin"), "the coin picked up before it came is not in its copy")
		assert_true(ca.has_node("vault/NetManager/Chest/Items/Gem"), "the rest of the level is")
	)
	assert_true(ok, "fixtures connected")


func test_keep_state_off_on_the_manager_gives_a_fresh_level() -> void:
	var vault := await _played_server()
	var m: NetManager = vault.get_node("NetManager")
	assert_eq(m.keep_state, NetManager.KEEP_DEFAULT, "the default follows the project setting")
	m.keep_state = NetManager.KEEP_OFF
	assert_true(net.server_session.unload_level(VAULT))
	assert_false(Array(net.server_session.kept_levels()).has(VAULT), "nothing kept")
	assert_eq(net.server_session.level_state(VAULT).size(), 0)
	assert_eq(_fresh(net.server_session.load_level(VAULT)), "")


func test_keep_state_off_in_the_project_settings_gives_a_fresh_level() -> void:
	ProjectSettings.set_setting(KEEP, false)
	var vault := await _played_server()
	assert_true(net.server_session.unload_level(VAULT))
	var again := net.server_session.load_level(VAULT)
	assert_eq(_fresh(again), "", "the setting turns it off")
	# A manager set to keep overrides the setting.
	again.get_node("NetManager").keep_state = NetManager.KEEP_ON
	again.get_node("NetManager/Chest").open = true
	await _frames(2)
	assert_true(net.server_session.unload_level(VAULT))
	assert_true(net.server_session.load_level(VAULT).get_node("NetManager/Chest").open, "KEEP_ON keeps it")


func test_reset_level_drops_the_kept_state() -> void:
	await _played_server()
	var s := net.server_session
	assert_true(s.unload_level(VAULT))
	assert_has(Array(s.kept_levels()), VAULT)
	assert_true(s.reset_level(VAULT), "there was a kept state")
	assert_false(s.reset_level(VAULT), "nothing left to drop")
	assert_eq(s.level_state(VAULT).size(), 0)
	var vault := s.load_level(VAULT)
	assert_eq(_fresh(vault), "", "a fresh vault")
	# On a loaded level it drops the state when the level unloads: the next load is fresh, this one
	# stays as it is.
	vault.get_node("NetManager/Chest").open = true
	assert_true(s.reset_level(VAULT))
	assert_true(vault.get_node("NetManager/Chest").open, "the loaded level is not touched")
	assert_true(s.unload_level(VAULT))
	assert_false(Array(s.kept_levels()).has(VAULT))
	assert_eq(_fresh(s.load_level(VAULT)), "")


func test_level_state_round_trips_into_a_new_session() -> void:
	await _played_server()
	var blob := net.server_session.level_state(VAULT)
	assert_gt(blob.size(), 6)
	net.teardown()
	net = null
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		f.server_session.empty_level_timeout = 1000.0
		var t0 := Time.get_ticks_usec()
		var vault := f.server_session.load_level(VAULT, null, blob)
		print("load_level with state: %d us" % (Time.get_ticks_usec() - t0))
		assert_eq(_kept(vault), "", "the blob restored the vault")
		for id in f.players():
			Net.spawn_for(id, PLAYER, vault.get_node("NetManager"), "P%d" % id)
		assert_true(await f.wait_until(func() -> bool: return ca.has_node("vault") and _kept(ca.get_node("vault")) == ""), "the client sees it")
	)
	assert_true(ok, "fixtures connected")


func test_the_state_argument_overrides_the_kept_state_and_follows_the_origin() -> void:
	await _played_server()
	var s := net.server_session
	var played := s.level_state(VAULT)
	assert_true(s.unload_level(VAULT))
	var fresh := s.load_level(VAULT, null, PackedByteArray())
	assert_eq(_fresh(fresh), "", "an empty state starts fresh whatever was kept")
	assert_true(s.unload_level(VAULT))
	var moved: Node2D = s.load_level(VAULT, Vector2(500, 0), played)
	assert_eq(_kept(moved, Vector2(500, 0)), "", "positions follow the level's new origin")


func test_a_bad_blob_is_refused_and_the_level_loads_fresh() -> void:
	allow_errors = true
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	net.start()
	assert_true(await net.wait_connected())
	var s := net.server_session
	var lobby_state := s.level_state(LOBBY)
	assert_eq(lobby_state.size(), 0, "nothing kept, not loaded")
	s.load_level(LOBBY)
	lobby_state = s.level_state(LOBBY)
	assert_gt(lobby_state.size(), 6)
	assert_true(s.unload_level(LOBBY))
	var vault := s.load_level(VAULT, null, PackedByteArray([1, 2, 3, 4, 5, 6, 7]))
	assert_not_null(vault, "the level loads")
	assert_eq(_fresh(vault), "", "fresh after a refused blob")
	assert_true(s.unload_level(VAULT))
	assert_true(s.reset_level(VAULT))
	vault = s.load_level(VAULT, null, lobby_state)
	assert_eq(_fresh(vault), "", "a blob of another scene is refused")
	assert_true(s.unload_level(VAULT))
	assert_true(s.reset_level(VAULT))
	var future := lobby_state.duplicate()
	future[4] = 99
	vault = s.load_level(VAULT, null, future)
	assert_eq(_fresh(vault), "", "an unknown version is refused")


func test_level_state_works_offline() -> void:
	assert_eq(Net.offline(), OK)
	offline_world = Node2D.new()
	offline_world.name = "OfflineWorld"
	add_child(offline_world)
	Net.set_world(offline_world)
	Net.empty_level_timeout = 1000.0
	var vault := Net.load_level(VAULT)
	assert_not_null(vault)
	_play(vault)
	await _frames(3)
	var blob := Net.level_state(VAULT)
	assert_gt(blob.size(), 6, "the save path: the current state as bytes")
	assert_true(Net.unload_level(VAULT))
	assert_has(Array(Net.kept_levels()), VAULT)
	assert_eq(_kept(Net.load_level(VAULT)), "", "kept offline too")
	assert_true(Net.unload_level(VAULT))
	assert_true(Net.reset_level(VAULT))
	assert_eq(_fresh(Net.load_level(VAULT)), "", "reset offline")
	assert_true(Net.unload_level(VAULT))
	Net.leave()
	# A new offline session (a loaded save game) starts from the saved bytes.
	assert_eq(Net.offline(), OK)
	assert_eq(Array(Net.kept_levels()), [], "a new session keeps nothing from the last one")
	assert_eq(_kept(Net.load_level(VAULT, null, blob)), "", "loaded from the save")
