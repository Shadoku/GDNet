extends TestCase
## WP8: diagnostics (docs/PLAN_NETCODE.md 1 "What the developer sees", 2.1 "Dirty tracking", 2.4 `Hash`,
## 3.1 `NetMeter` / `NetEmulation`, 3.4 `gdnet/debug/*`, gate G0): the meter and its counters against
## the pipe's byte counts, hash telemetry and the divergence counter, the ownership overlay and its rows,
## the emulation preset setting, the editor plugin class. Headless over NetPipePeer; no pixels.

const CRATE := preload("res://tests/fixtures/net/crate.tscn")
const PLAYER := preload("res://tests/fixtures/net/owned_player.tscn")
const BOX := preload("res://tests/fixtures/net/owned_box.tscn")
const SETTINGS: Array[String] = [
	"gdnet/debug/meter", "gdnet/debug/overlay", "gdnet/debug/hash_telemetry", "gdnet/debug/emulation_preset",
]

var net: NetFixture
var _saved: Dictionary = {}


func before_each() -> void:
	for s in SETTINGS:
		_saved[s] = ProjectSettings.get_setting(s) if ProjectSettings.has_setting(s) else null


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null
	Net.show_meter(false)
	Net.show_overlay(false)
	Net.leave()
	Net.emulation = null
	for s in SETTINGS:
		ProjectSettings.set_setting(s, _saved[s])


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _start(clients: int = 1, mode: NetFixture.HostMode = NetFixture.HostMode.DEDICATED) -> Dictionary:
	net = NetFixture.new(self, clients, mode)
	net.start()
	assert_true(await net.wait_connected())
	return net.add_managers()


## Adds `scene` named `node_name` under the server's manager (owned by `owner`) and returns its copies
## by branch name once every client has it.
func _spawn(managers: Dictionary, scene: PackedScene, node_name: String, owner: int = 1) -> Dictionary:
	var node: Node = scene.instantiate()
	node.name = node_name
	Net.set_owner(node, owner)
	managers["Server"].add_child(node)
	var out := {"Server": node}
	for c in net.clients:
		var path := "World/" + node_name
		assert_true(await net.wait_until(func() -> bool: return c.has_node(path) and Net.net_id(c.get_node(path)) > 0), "the copy reached " + String(c.name))
		out[String(c.name)] = c.get_node(path)
	return out


func _sum(a: PackedInt64Array) -> int:
	var t := 0
	for x in a:
		t += x
	return t


func test_meter_counters_match_the_pipe_and_its_labels_update() -> void:
	var managers := await _start()
	var copies := await _spawn(managers, CRATE, "M")
	var client: Node = net.clients[0]
	var pipe: NetPipePeer = net.peer(client)
	Net.show_meter(true)
	assert_true(Net.meter_visible())
	await _frames(1)
	var meter: CanvasLayer = get_tree().root.get_node("__NetMeter")
	assert_true(meter is NetMeter, "the meter is a NetMeter CanvasLayer")
	meter.session = net.client_sessions[0]
	var labels: Array = meter.find_children("*", "Label", true, false)
	assert_gt(labels.size(), 10, "one label per figure")
	var before_text: String = meter.text()
	var s0: Dictionary = net.client_sessions[0].stats()
	var p0 := pipe.bytes_received() - pipe.packets_received()
	for i in 60:
		copies["Server"].position.x += 1.5
		await get_tree().physics_frame
	var s1: Dictionary = net.client_sessions[0].stats()
	var p1 := pipe.bytes_received() - pipe.packets_received()
	var gd := _sum(s1["bytes_received_total"]) - _sum(s0["bytes_received_total"])
	var raw := p1 - p0
	assert_gt(gd, 500, "snapshots arrived")
	assert_true(absi(gd - raw) <= 64, "gdnet counted %d bytes, the pipe carried %d (minus SceneMultiplayer's byte per packet)" % [gd, raw])
	assert_true(await net.wait_until(func() -> bool: return meter.last_stats.get("tick", 0) > s1["tick"]), "the meter refreshed")
	assert_true(_sum(meter.last_stats["bytes_received_total"]) >= _sum(s1["bytes_received_total"]), "the meter shows the session's counters")
	assert_ne(meter.text(), before_text, "the labels changed")
	assert_true(meter.text().contains("RTT"), meter.text())
	Net.show_meter(false)
	assert_false(Net.meter_visible())
	await _frames(1)
	assert_null(get_tree().root.get_node_or_null("__NetMeter"), "hiding removes the layer")


func test_meter_setting_shows_it_when_a_session_starts() -> void:
	ProjectSettings.set_setting("gdnet/debug/meter", true)
	assert_false(Net.meter_visible(), "nothing before a session (G0)")
	assert_eq(Net.offline(), OK)
	await _frames(3)
	assert_true(Net.meter_visible())
	assert_not_null(get_tree().root.get_node_or_null("__NetMeter"))
	Net.leave()


func test_a_held_state_edit_raises_divergences_and_an_untouched_replica_stays_zero() -> void:
	var managers := await _start()
	var a := await _spawn(managers, CRATE, "A")
	var b := await _spawn(managers, CRATE, "B")
	var session: NetSession = net.client_sessions[0]
	for i in 30:
		a["Server"].position.x += 2.0
		b["Server"].position.y += 1.0
		await get_tree().physics_frame
	assert_gt(session.stats()["hashes_received"], 0, "hashes arrive on channel 2")
	assert_gt(session.stats()["hashes_compared"], 0)
	assert_eq(Net.divergences(a["ClientA"]), 0, "matching state does not diverge")
	assert_eq(session.stats()["divergences"], 0)
	# The client's held rotation (which the server does not resend while it does not change) goes
	# wrong; the moving entity's hashes keep arriving and now differ.
	assert_true(session._debug_corrupt_held(a["ClientA"], "rotation", 2.5))
	var moving := func() -> bool:
		a["Server"].position.x += 2.0
		b["Server"].position.y += 1.0
		return Net.divergences(a["ClientA"]) > 0
	assert_true(await net.wait_until(moving, 2.0), "the edited replica diverges")
	for i in 20:
		b["Server"].position.y += 1.0
		await get_tree().physics_frame
	assert_eq(Net.divergences(b["ClientA"]), 0, "the untouched one does not")
	assert_gt(session.stats()["divergences"], 0)
	assert_true(session.is_online(), "telemetry never halts")


func test_hash_telemetry_off_sends_no_hashes() -> void:
	ProjectSettings.set_setting("gdnet/debug/hash_telemetry", false)
	var managers := await _start()
	var a := await _spawn(managers, CRATE, "H")
	for i in 20:
		a["Server"].position.x += 2.0
		await get_tree().physics_frame
	assert_eq(net.server_session.stats()["hashes_sent"], 0)
	assert_eq(net.client_sessions[0].stats()["hashes_received"], 0)


func _row(rows: Array, node_name: String, branch: Node) -> Dictionary:
	var id := Net.net_id(branch.get_node("World/" + node_name))
	for r: Dictionary in rows:
		if r["net_id"] == id:
			return r
	return {}


func test_overlay_rows_and_layer_show_roles_per_peer() -> void:
	var managers := await _start(2)
	var a := net.peer_id(net.clients[0])
	await _spawn(managers, CRATE, "Crate")
	await _spawn(managers, PLAYER, "PA", a)
	await _spawn(managers, BOX, "Box", a)
	var ca: Node = net.clients[0]
	var cb: Node = net.clients[1]
	var srows: Array = net.server_session.overlay_rows()
	var arows: Array = net.client_sessions[0].overlay_rows()
	var brows: Array = net.client_sessions[1].overlay_rows()
	assert_eq(srows.size(), 3, "every managed entity on the server")
	assert_eq(arows.size(), 3)
	assert_eq(_row(srows, "Crate", net.server)["role"], "server")
	assert_eq(_row(arows, "Crate", ca)["role"], "server", "server-owned is white on clients too")
	assert_eq(_row(arows, "PA", ca)["role"], "own", "the client's own player is green on it")
	assert_eq(_row(brows, "PA", cb)["role"], "other", "another client's player is grey")
	assert_eq(_row(srows, "PA", net.server)["role"], "other")
	assert_eq(_row(arows, "Box", ca)["role"], "client_owned", "client_owned is amber (the trust label)")
	assert_eq(_row(brows, "Box", cb)["role"], "client_owned")
	var row := _row(arows, "PA", ca)
	assert_eq(row["owner"], a)
	assert_eq(row["scene"], PLAYER.resource_path)
	assert_eq(row["hashes_diverged"], 0)
	assert_true(row.has("client_owned") and row.has("transfer") and row.has("parts") and row.has("color"))
	# A rejected write turns the entity red on the writer and on the server for a second.
	ca.get_node("World/Crate").z_index = 5
	assert_true(await net.wait_until(func() -> bool:
		return _row(net.client_sessions[0].overlay_rows(), "Crate", ca)["role"] == "rejected"), "red on the writer")
	assert_eq(_row(net.server_session.overlay_rows(), "Crate", net.server)["role"], "rejected", "red on the server")
	# The in-game layer: one label per entity of every session in the tree.
	Net.show_overlay(true)
	assert_true(Net.overlay_visible())
	await _frames(2)
	var layer: Node = get_tree().root.get_node("__NetOverlay")
	assert_true(layer is CanvasLayer)
	var labels: Array = layer.find_children("*", "Label", true, false)
	var shown := 0
	for l: Label in labels:
		if l.visible:
			shown += 1
	assert_eq(shown, 9, "three entities on three peers")
	Net.show_overlay(false)
	assert_false(Net.overlay_visible())


func test_emulation_preset_setting_applies_at_session_start() -> void:
	ProjectSettings.set_setting("gdnet/debug/emulation_preset", "bad")
	assert_eq(Net.offline(), OK)
	var e: NetEmulation = Net.emulation
	assert_not_null(e, "the preset made an emulation")
	assert_eq(e.send_latency_ms, 100.0)
	assert_eq(e.receive_loss, 0.05)
	Net.leave()
	Net.emulation = null
	assert_true(Net.emulation_preset("mobile"))
	assert_eq(Net.emulation.send_latency_ms, 120.0)
	assert_eq(Net.emulation.send_variance_ms, 20.0)
	assert_true(Net.emulation_preset("lan"))
	assert_eq(Net.emulation.send_latency_ms, 5.0)
	assert_false(Net.emulation_preset("dialup"))


func test_editor_plugin_class_exists_and_g0_without_a_session() -> void:
	assert_true(ClassDB.class_exists("GdNetEditorPlugin"), "the editor debugger plugin is registered")
	var before := NetSession.packets_sent_total()
	assert_false(Net.meter_visible())
	assert_false(Net.overlay_visible())
	assert_eq(Net.overlay_rows(), [])
	assert_eq(Net.stats(), {})
	var lone := Node2D.new()
	add_child(lone)
	assert_eq(Net.divergences(lone), 0)
	await _frames(3)
	assert_null(get_tree().root.get_node_or_null("__NetMeter"))
	assert_null(get_tree().root.get_node_or_null("__NetOverlay"))
	assert_eq(NetSession.packets_sent_total(), before, "no packets (G0)")
