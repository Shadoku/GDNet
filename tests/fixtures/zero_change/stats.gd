extends Node2D
## A plain single-player node with three exported values that change over time, shown on the level's
## label. Gate G7's fixture: no multiplayer code here.

@export var count: int = 0
@export var ratio: float = 0.0
@export var title: String = "start"

var _ticks: int = 0


func _ready() -> void:
	count = randi() % 1000
	ratio = randf() * 100.0
	title = "level %d" % (count / 5)


func _physics_process(delta: float) -> void:
	ratio += delta * 0.5
	_ticks += 1
	if _ticks % 30 == 0:
		count += 1
		title = "level %d" % (count / 5)
		var label := get_node_or_null("../Score") as Label
		if label != null:
			label.text = "Score %d" % count
