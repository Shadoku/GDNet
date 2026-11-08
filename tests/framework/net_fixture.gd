class_name NetFixture
extends RefCounted
## Headless multi-peer test fixture (docs/PLAN_NETCODE.md 4, "Headless two-peer testing").
##
## Builds /root-level branches `Server`, `ClientA`, `ClientB`, ... Each branch gets its own
## `MultiplayerAPI` (`SceneTree.set_multiplayer(api, "/root/<branch>")`) and its own gdnet session: a
## `NetSession` node, child of the branch, with the same surface as the `Net` singleton (`tick()`,
## `rtt()`, `peers()`, `is_online()`, the signals). The `Net` singleton is process-global and always
## serves the default (whole-tree) multiplayer, so branches never go through it; a test may still use
## `Net` beside the branches (it is a separate session on the tree's default API).
##
## The branches are connected in memory by `NetPipePeer` ends (the server end belongs to `Server`), with
## optional `NetEmulation` per branch. Typical use inside a `TestCase`:
##
##     var net := NetFixture.new(self, 2)
##     net.start()
##     assert_true(await net.wait_connected())
##     ...                      # net.server, net.clients[0], net.session(net.clients[0]) ...
##     net.teardown()           # also safe to call from after_each
##
## Host modes (gate G2): `HOST_PLAYER` (the server branch is also a player, peer 1) and `DEDICATED`
## (the server branch only serves). Later packages spawn players per `players()`;
## `NetFixture.run_both_modes(test, n, scenario)` runs one scenario under each.

enum HostMode { HOST_PLAYER, DEDICATED }

const CLIENT_NAMES: Array[String] = ["ClientA", "ClientB", "ClientC", "ClientD", "ClientE", "ClientF"]

var host_mode: HostMode = HostMode.HOST_PLAYER
## The server branch node (`/root/Server`).
var server: Node
## The client branch nodes, in order.
var clients: Array[Node] = []
var server_session: NetSession
var client_sessions: Array[NetSession] = []
## Peer ids each branch saw join, by branch name (filled from the sessions' `peer_joined`).
var joined: Dictionary = {}
## Peer ids each branch saw leave, by branch name.
var left: Dictionary = {}
## Disconnection reasons by branch name.
var disconnect_reasons: Dictionary = {}

var _test: Node
var _client_count: int
var _server_peer: NetPipePeer
var _client_peers: Array[NetPipePeer] = []
var _started: bool = false


func _init(test: Node, client_count: int = 1, mode: HostMode = HostMode.HOST_PLAYER) -> void:
	_test = test
	_client_count = clampi(client_count, 0, CLIENT_NAMES.size())
	host_mode = mode
	build()


## Creates the branches, their sessions and the pipe ends, without starting anything. Called by
## `_init`; set session properties (e.g. `_debug_protocol_salt`) or emulation, then call `start()`.
func build() -> void:
	if server != null:
		return
	var root := _test.get_tree().root
	server = _make_branch(root, "Server")
	server_session = _make_session(server)
	_server_peer = NetPipePeer.create_server()
	for i in _client_count:
		var branch := _make_branch(root, CLIENT_NAMES[i])
		clients.append(branch)
		client_sessions.append(_make_session(branch))
		_client_peers.append(NetPipePeer.create_client(_server_peer))


## Starts every session: the server first, then each client.
func start() -> void:
	if _started:
		return
	_started = true
	server_session.start(_server_peer, server.get_path())
	for i in clients.size():
		client_sessions[i].start(_client_peers[i], clients[i].get_path())


## Awaits physics frames until every client finished the handshake and the server sees all of them.
## Returns false on timeout.
func wait_connected(timeout: float = 5.0) -> bool:
	return await wait_until(func() -> bool: return _all_connected(), timeout)


## Awaits physics frames until `condition` returns true; false on timeout.
func wait_until(condition: Callable, timeout: float = 5.0) -> bool:
	var deadline := Time.get_ticks_msec() + int(timeout * 1000.0)
	while not condition.call():
		if Time.get_ticks_msec() > deadline:
			return false
		await _test.get_tree().physics_frame
	return true


## The session of a branch node (`server` or one of `clients`).
func session(branch: Node) -> NetSession:
	if branch == server:
		return server_session
	var i := clients.find(branch)
	return client_sessions[i] if i >= 0 else null


## The pipe end of a branch node.
func peer(branch: Node) -> NetPipePeer:
	if branch == server:
		return _server_peer
	var i := clients.find(branch)
	return _client_peers[i] if i >= 0 else null


## The peer id of a branch (1 for the server; a client's transport id).
func peer_id(branch: Node) -> int:
	var p := peer(branch)
	return p.get_unique_id() if p != null else 0


## Applies network emulation to one branch's pipe end (its send_* values to what it sends, receive_* to
## what it receives). Null removes it. Works before or after `start()`.
func set_emulation(branch: Node, emulation: NetEmulation) -> void:
	session(branch).set_emulation(emulation)
	peer(branch).set_emulation(emulation)


## The peer ids that act as players: in HOST_PLAYER mode the server (1) and every client, in DEDICATED
## mode the clients only.
func players() -> PackedInt32Array:
	var ids := PackedInt32Array()
	if host_mode == HostMode.HOST_PLAYER:
		ids.append(1)
	for branch in clients:
		ids.append(peer_id(branch))
	return ids


## Frees the branches, clients first. Each branch's nodes leave the tree while gdnet's API is still
## installed (so stock synchronizers and spawners unregister from the API they registered with); the
## session node, the branch's first child, exits last and leaves its session, restoring the tree's
## multiplayer for the branch.
func teardown() -> void:
	for branch in clients:
		if is_instance_valid(branch):
			branch.free()
	if is_instance_valid(server):
		server.free()
	clients.clear()
	client_sessions.clear()
	_client_peers.clear()
	_server_peer = null
	server = null
	server_session = null


## Runs `scenario` (a Callable taking a started, connected NetFixture and awaiting as it likes) once per
## host mode, tearing down in between. Returns false when a fixture failed to connect.
static func run_both_modes(test: Node, client_count: int, scenario: Callable) -> bool:
	for mode: HostMode in [HostMode.HOST_PLAYER, HostMode.DEDICATED]:
		var f := NetFixture.new(test, client_count, mode)
		f.start()
		var ok: bool = await f.wait_connected()
		if ok:
			await scenario.call(f)
		f.teardown()
		if not ok:
			return false
	return true


func _all_connected() -> bool:
	if not is_instance_valid(server_session) or not server_session.is_online():
		return false
	if server_session.peers().size() != clients.size():
		return false
	for s in client_sessions:
		if not s.is_online() or not s.peers().has(1):
			return false
	return true


func _make_branch(root: Node, branch_name: String) -> Node:
	var stale := root.get_node_or_null(branch_name)
	if stale != null:
		stale.free()
	var branch := Node.new()
	branch.name = branch_name
	root.add_child(branch)
	return branch


func _make_session(branch: Node) -> NetSession:
	var s := NetSession.new()
	s.name = "NetSession"
	branch.add_child(s)
	var key := String(branch.name)
	joined[key] = []
	left[key] = []
	s.peer_joined.connect(func(id: int) -> void: joined[key].append(id))
	s.peer_left.connect(func(id: int) -> void: left[key].append(id))
	s.disconnected.connect(func(reason: String) -> void: disconnect_reasons[key] = reason)
	return s
