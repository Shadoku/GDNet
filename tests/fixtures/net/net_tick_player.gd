extends CharacterBody2D
## Test fixture (WP4): a player whose step is `_net_tick` (rung 4); `_physics_process` is defined too
## and must not run on the server or the owner.
const SPEED := 200.0

var physics_calls: int = 0
var net_ticks: int = 0
var fresh_ticks: int = 0
var last_tick: int = -1


func _physics_process(_delta: float) -> void:
	physics_calls += 1


func _net_tick(_delta: float, tick: int, is_fresh: bool) -> void:
	net_ticks += 1
	last_tick = tick
	if is_fresh:
		fresh_ticks += 1
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
