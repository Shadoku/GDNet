class_name DoorsChest
extends Area2D
## A chest that opens when a player walks into it, on the server. `open` is exported, so it
## replicates, and the server keeps it when the level unloads: the chest is still open next time.

const SHUT := Color(0.55, 0.35, 0.15, 1)
const OPENED := Color(1, 0.85, 0.3, 1)

## Whether the lid is open.
@export var open: bool = false:
	set(value):
		open = value
		_show()


func _ready() -> void:
	body_entered.connect(_on_body_entered)
	_show()


func _show() -> void:
	if is_node_ready():
		($Lid as Polygon2D).color = OPENED if open else SHUT


func _on_body_entered(body: Node2D) -> void:
	if multiplayer.is_server() and body is DoorsPlayer:
		open = true
