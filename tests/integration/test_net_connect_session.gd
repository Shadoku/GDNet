extends TestCase
## WP23 (docs/PLAN_NETCODE.md 12.10): the connect layer and the shared title screen with real gdnet
## sessions. A join code made by one NetConnect is resolved by another through FakeSteamBackend, whose
## peers are a NetPipePeer pair, and the two NetFixture branches finish gdnet's handshake. The menu shows
## Steam only where it is available and routes its one field to a code or an address.

const MENU := preload("res://addons/gdnet/connect/connect_menu.tscn")

var net: NetFixture


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null


func _connector(world: Dictionary, id: int, s: NetSession, root: Node) -> NetConnect:
	var c := NetConnect.new()
	c.backend = FakeSteamBackend.new(world, id)
	c.session = s
	c.session_root = root
	add_child(c)
	return c


func test_a_join_code_connects_two_gdnet_sessions() -> void:
	net = NetFixture.new(self, 1)
	net.build()
	var world := {}
	var host := _connector(world, 1001, net.server_session, net.server)
	var client := _connector(world, 2002, net.client_sessions[0], net.clients[0])
	var failures: Array[String] = []
	host.failed.connect(func(r: String) -> void: failures.append(r))
	client.failed.connect(func(r: String) -> void: failures.append(r))
	assert_eq(host.host_steam(), OK)
	assert_true(await await_signal(host.hosted, 2.0), "the host's lobby exists")
	assert_eq(host.code().length(), 6)
	assert_eq(client.join_steam(host.code()), OK)
	assert_true(await await_signal(client.joined, 2.0), "the client found the code")
	assert_true(await net.wait_connected(), "gdnet's handshake completed over the Steam (fake) peer")
	assert_eq(net.server_session.peers().size(), 1)
	assert_eq(net.client_sessions[0].peers(), PackedInt32Array([1]))
	assert_eq(failures, [])
	client.leave()
	assert_false(net.client_sessions[0].is_online(), "leave() ends the client's session")
	host.leave()
	assert_false(net.server_session.is_online())
	assert_eq(world["lobbies"].size(), 0, "and the lobby")


func _menu(backend: NetSteamBackend) -> NetConnectMenu:
	var m: NetConnectMenu = MENU.instantiate()
	m.title = "Test"
	var c: NetConnect = m.get_node("Connect")
	c.backend = backend
	var started: Array[MultiplayerPeer] = []
	m.set_meta("started", started)
	c.start_peer = func(peer: MultiplayerPeer) -> Error:
		started.append(peer)
		return OK
	add_child(m)
	return m


func test_the_menu_offers_steam_only_where_it_is_available() -> void:
	var absent := NetSteamBackend.new()
	absent.singleton_name = "NoSuchSteamSingleton"
	var m := _menu(absent)
	assert_eq((m.get_node("Title") as Label).text, "Test")
	assert_false((m.get_node("HostSteam") as Control).visible, "no Steam: no Steam button")
	assert_true((m.get_node("Status") as Label).text.contains("GodotSteam"), "the status line says why")
	var m2 := _menu(FakeSteamBackend.new())
	assert_true((m2.get_node("HostSteam") as Control).visible)


func test_hosting_on_steam_from_the_menu_shows_the_code() -> void:
	var m := _menu(FakeSteamBackend.new({}, 1001))
	var runs: Array[int] = [0]
	m.started.connect(func() -> void: runs[0] += 1)
	(m.get_node("HostSteam") as Button).pressed.emit()
	assert_true(await await_signal(m.started, 2.0))
	assert_eq(runs[0], 1)
	var code := m.connector.code()
	assert_true(NetConnect.is_join_code(code))
	assert_true((m.get_node("CodeRow") as Control).visible)
	assert_true((m.get_node("CodeRow/Code") as Label).text.contains(code))
	assert_false((m.get_node("Host") as Control).visible, "the buttons are gone once a session runs")
	assert_eq((m.get_meta("started") as Array).size(), 1)


func test_the_join_field_takes_a_code_and_offline_needs_no_network() -> void:
	var world := {}
	var host := _menu(FakeSteamBackend.new(world, 1001))
	(host.get_node("HostSteam") as Button).pressed.emit()
	assert_true(await await_signal(host.started, 2.0))
	var client := _menu(FakeSteamBackend.new(world, 2002))
	(client.get_node("Address") as LineEdit).text = host.connector.code().to_lower()
	(client.get_node("Join") as Button).pressed.emit()
	assert_true(await await_signal(client.started, 2.0), "a code in the field joins on Steam")
	assert_false((client.get_node("CodeRow") as Control).visible, "a client has no code to show")
	var alone := _menu(FakeSteamBackend.new())
	(alone.get_node("Offline") as Button).pressed.emit()
	assert_eq((alone.get_meta("started") as Array).size(), 1)
	assert_true((alone.get_meta("started") as Array)[0] is NetPipePeer, "offline is a NetPipePeer server end")


## WP17: a Steam player whose game ends and starts again joins with the same Steam id, so it takes its
## seat back; the host hosts from a saved session state through `NetConnect.state`.
func test_a_steam_player_who_restarts_gets_its_seat_back() -> void:
	var saved_on := NetFixture.new(self, 0)
	saved_on.start_server()
	saved_on.server_session.state.set("run/floor", 4)
	var saved := saved_on.server_session.save_state()
	saved_on.teardown()
	net = NetFixture.new(self, 1)
	net.build()
	net.server_session.rejoin_timeout_s = -1.0
	var world := {}
	var host := _connector(world, 1001, net.server_session, net.server)
	host.state = saved
	var client := _connector(world, 2002, net.client_sessions[0], net.clients[0])
	host.host_steam()
	assert_true(await await_signal(host.hosted, 2.0))
	assert_eq(net.server_session.state.get("run/floor"), 4, "hosted from the saved state")
	client.join_steam(host.code())
	assert_true(await net.wait_connected())
	assert_eq(net.client_sessions[0].key, "steam:2002", "the Steam id is the seat key")
	var seat := net.client_sessions[0].my_seat()
	assert_gt(seat, 0)
	# The player's game ends (its connector and branch go with it) and starts again.
	client.free()
	net.drop_client(0)
	assert_true(await net.wait_until(func() -> bool: return net.server_session.peer_of(seat) == 0), "the seat is away")
	var branch := Node.new()
	branch.name = "ClientA"
	get_tree().root.add_child(branch)
	var s := NetSession.new()
	branch.add_child(s)
	var again := _connector(world, 2002, s, branch)
	again.join_steam(host.code())
	assert_true(await net.wait_until(func() -> bool: return s.is_online() and s.my_seat() > 0), "joined again")
	assert_eq(s.my_seat(), seat, "the same seat")
	again.leave()
	branch.free()
