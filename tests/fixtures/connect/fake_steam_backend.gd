class_name FakeSteamBackend
extends NetSteamBackend
## A Steam stand-in for NetConnect's tests (WP23, docs/PLAN_NETCODE.md 12.10). Several fakes share one
## `world` Dictionary (pass the same one to each), which holds the lobbies, so a host's fake and a client's
## fake see the same "Steam". Like Steam, answers arrive on `poll()` (NetConnect's `advance()`), never
## during the call that asked. `silent` swallows every request (the timeout tests).
##
## Peers: `host_peer()` returns a `NetPipePeer` server end and records it in the world under the host's
## id; `client_peer(owner)` a client end joined to that server. A test can therefore go all the way to a
## connected gdnet session through NetConnect.

## This fake's Steam user id.
var me: int = 1001
## What `start()` returns: empty = Steam is usable here.
var start_reason: String = ""
## When true, requests are accepted and never answered.
var silent: bool = false
## When not empty, `create_lobby` fails with this reason.
var create_error: String = ""
## When not empty, every `join_lobby` fails with this reason.
var join_error: String = ""
## When not empty, `host_peer()` returns null with this reason.
var peer_error: String = ""
## The app id passed to `start()` (-1 before).
var started_with: int = -1
## The peers this fake made, in order.
var peers: Array[MultiplayerPeer] = []
## The shared world: `lobbies` (id -> {owner, data, members, max}), `next_id`, `backends` (id -> a WeakRef to the fake, so no reference cycle),
## `servers` (owner id -> NetPipePeer).
var world: Dictionary

var _queue: Array[Array] = []


func _init(shared: Dictionary = {}, id: int = 1001) -> void:
	world = shared
	me = id
	if not world.has("lobbies"):
		world["lobbies"] = {}
		world["next_id"] = 500
		world["backends"] = {}
		world["servers"] = {}
	world["backends"][me] = weakref(self)


func start(app_id: int) -> String:
	started_with = app_id
	return start_reason


func poll() -> void:
	var due := _queue.duplicate()
	_queue.clear()
	for e: Array in due:
		var sig: Signal = e[0]
		var args: Array = [sig.get_name()]
		args.append_array(e[1])
		callv("emit_signal", args)


func my_id() -> int:
	return me


func create_lobby(max_members: int) -> void:
	if silent:
		return
	if not create_error.is_empty():
		_later(lobby_created, [0, create_error])
		return
	var id: int = world["next_id"]
	world["next_id"] = id + 1
	world["lobbies"][id] = {"owner": me, "data": {}, "members": [me], "max": max_members}
	_later(lobby_created, [id, ""])


func set_lobby_data(lobby_id: int, key: String, value: String) -> void:
	if world["lobbies"].has(lobby_id):
		world["lobbies"][lobby_id]["data"][key] = value


func lobby_data(lobby_id: int, key: String) -> String:
	if not world["lobbies"].has(lobby_id):
		return ""
	return String(world["lobbies"][lobby_id]["data"].get(key, ""))


func find_lobbies(filters: Dictionary) -> void:
	if silent:
		return
	var found: Array = []
	for id: int in world["lobbies"]:
		var data: Dictionary = world["lobbies"][id]["data"]
		var ok := true
		for key: String in filters:
			if String(data.get(key, "")) != String(filters[key]):
				ok = false
		if ok:
			found.append(id)
	_later(lobby_list, [found])


func join_lobby(lobby_id: int) -> void:
	if silent:
		return
	if not join_error.is_empty():
		_later(lobby_joined, [lobby_id, join_error])
		return
	if not world["lobbies"].has(lobby_id):
		_later(lobby_joined, [lobby_id, "the game no longer exists"])
		return
	var lobby: Dictionary = world["lobbies"][lobby_id]
	if lobby["members"].size() >= int(lobby["max"]):
		_later(lobby_joined, [lobby_id, "the game is full"])
		return
	lobby["members"].append(me)
	_later(lobby_joined, [lobby_id, ""])


func leave_lobby(lobby_id: int) -> void:
	if not world["lobbies"].has(lobby_id):
		return
	var lobby: Dictionary = world["lobbies"][lobby_id]
	lobby["members"].erase(me)
	if lobby["members"].is_empty():
		world["lobbies"].erase(lobby_id)
		return
	# Steam passes ownership to a remaining member; the lobby and its data stay.
	if lobby["owner"] == me:
		lobby["owner"] = lobby["members"][0]
	for member: int in lobby["members"]:
		var ref: WeakRef = world["backends"].get(member)
		var other: FakeSteamBackend = null if ref == null else ref.get_ref()
		if other != null:
			other._later(other.lobby_member_left, [lobby_id, me])


func lobby_owner(lobby_id: int) -> int:
	if not world["lobbies"].has(lobby_id):
		return 0
	return int(world["lobbies"][lobby_id]["owner"])


func host_peer() -> MultiplayerPeer:
	if not peer_error.is_empty():
		last_error = peer_error
		return null
	var peer := NetPipePeer.create_server()
	world["servers"][me] = peer
	peers.append(peer)
	return peer


func client_peer(owner_id: int) -> MultiplayerPeer:
	var server: NetPipePeer = world["servers"].get(owner_id)
	if server == null:
		last_error = "nobody hosts as %d" % owner_id
		return null
	var peer := NetPipePeer.create_client(server)
	peers.append(peer)
	return peer


## Test helper: a lobby made by someone else (another game, a stale or duplicate code).
func add_lobby(owner_id: int, data: Dictionary, max_members: int = 16) -> int:
	var id: int = world["next_id"]
	world["next_id"] = id + 1
	world["lobbies"][id] = {"owner": owner_id, "data": data.duplicate(), "members": [owner_id], "max": max_members}
	return id


## Test helper: Steam reports a "join game" request from the friends list or an invite.
func request_join(lobby_id: int) -> void:
	_later(join_requested, [lobby_id])


func _later(sig: Signal, args: Array) -> void:
	_queue.append([sig, args])
