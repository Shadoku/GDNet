class_name DoorsWorld
extends Node2D
## The World node: every level this peer shows is loaded under it. The server holds every level
## someone needs; a client holds its player's level and that level's neighbours
## (docs/guide/09_rooms_and_travel.md).

const PLAYER := preload("res://demos/doors/player.tscn")

## The level new players start in.
@export_file("*.tscn") var start_level: String = "res://demos/doors/chunk_0_0.tscn"
## True on a dedicated server: peer 1 gets no player.
@export var dedicated: bool = false

var _players: Dictionary = {}


func _ready() -> void:
	Net.set_world(self)


## Called once the session runs. The server loads the start level, opens the chat to everyone and
## gives every peer a player; a client waits for the server to tell it where its player is.
func start() -> void:
	var session: NetSession = Net.session_for(self)
	if not session.is_server():
		return
	session.load_level(start_level)
	session.state.allow("chat/", "all")
	session.peer_joined.connect(spawn_player)
	session.peer_left.connect(remove_player)
	if not dedicated:
		spawn_player(1)
	for id in session.peers():
		spawn_player(id)


## Server: a player for peer `id`, at the start level's `Entrance`.
func spawn_player(id: int) -> void:
	var level: Node = Net.session_for(self).load_level(start_level)
	var player: Node2D = Net.spawn_for(id, PLAYER, level.get_node("NetManager"), "Player%d" % id)
	player.global_position = (level.get_node("Entrance") as Node2D).global_position
	_players[id] = player


func remove_player(id: int) -> void:
	if is_instance_valid(_players.get(id)):
		_players[id].queue_free()
	_players.erase(id)
