class_name BallpitHud
extends VBoxContainer
## The controls: ball count, the snapshot budget, trusted paddles, a shake, the meter. They change the
## server's world, so only the host can use them; a client sees them greyed out.

## The pit the controls act on.
@export var pit: BallpitSpawner


func _ready() -> void:
	$Balls.value_changed.connect(func(v: float) -> void:
		pit.ball_count = int(v)
		_show_values())
	$Budget.value_changed.connect(func(v: float) -> void:
		Net.budget_bytes_per_s = int(v)
		_show_values())
	$Trusted.toggled.connect(pit.set_trusted_paddles)
	$Shake.pressed.connect(pit.shake)
	$Meter.toggled.connect(Net.show_meter)


## Called once the session runs.
func start() -> void:
	var session: NetSession = Net.session_for(pit)
	var host := session.is_server()
	$Balls.editable = host
	$Budget.editable = host
	$Trusted.disabled = not host
	$Shake.disabled = not host
	$Balls.set_value_no_signal(pit.ball_count)
	$Budget.set_value_no_signal(Net.budget_bytes_per_s)
	Net.show_meter($Meter.button_pressed)
	_show_values()


func _show_values() -> void:
	$BallsLabel.text = "Balls: %d" % pit.ball_count
	$BudgetLabel.text = "Budget: %d B/s per peer" % int($Budget.value)
