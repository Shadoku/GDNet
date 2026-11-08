extends Area2D
## A door. When a player walks in, the server sends that player to `target_scene`, where it arrives
## at the marker named `spawn_point`.

## The level this door leads to.
@export_file("*.tscn") var target_scene: String = ""
## The Marker2D of that level the player arrives at.
@export var spawn_point: String = "Entrance"


func _ready() -> void:
	body_entered.connect(_on_body_entered)


func _on_body_entered(body: Node2D) -> void:
	# Only the server moves players, and only players of this door's own level: a server holding
	# several levels has them all in one physics space.
	if multiplayer.is_server() and body is DoorsPlayer and owner.is_ancestor_of(body):
		Net.travel(body, target_scene, spawn_point)
