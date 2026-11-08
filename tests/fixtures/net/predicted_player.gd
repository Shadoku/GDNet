extends CharacterBody2D
## Test fixture (WP4): rung 1's "after" player of docs/PLAN_NETCODE.md section 1, copied verbatim, plus
## counters the tests read (they never change what the player does).
const SPEED := 200.0

## Every `_physics_process` call, fresh or replayed.
var physics_calls: int = 0
## `_physics_process` calls inside a replay (`Net.is_fresh()` false).
var replayed_calls: int = 0
## `_process` calls (never gated).
var process_calls: int = 0
## Fresh ticks on which "jump" was just pressed: the one-shot guarded by `Net.is_fresh()`.
var jumps: int = 0
## Ticks on which "jump" was just pressed, fresh or replayed.
var jump_edges: int = 0


func _physics_process(delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
	if Net.is_fresh() and velocity.length() > 0.0 and not $Steps.playing:
		$Steps.play()
	_count()


func _process(_delta: float) -> void:
	process_calls += 1


func _count() -> void:
	physics_calls += 1
	if not Net.is_fresh():
		replayed_calls += 1
	if Net.input(self).is_action_just_pressed("jump"):
		jump_edges += 1
		if Net.is_fresh():
			jumps += 1
