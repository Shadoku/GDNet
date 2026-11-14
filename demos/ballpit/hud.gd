class_name BallpitHud
extends VBoxContainer
## The controls: ball count, the snapshot budget, trusted paddles, who simulates the balls, a shake,
## the meter. They change the server's world, so only the host can use them; a client sees them greyed
## out. The prediction selector is the other way round: it changes only what a client's screen shows.

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
	for mode: String in BallpitSpawner.PHYSICS:
		$Physics.add_item(mode)
	$Physics.item_selected.connect(func(i: int) -> void: pit.set_ball_physics($Physics.get_item_text(i)))
	for mode: String in BallpitSpawner.PREDICTION:
		$Prediction.add_item(mode)
	$Prediction.item_selected.connect(func(i: int) -> void: pit.set_ball_prediction($Prediction.get_item_text(i)))
	$Shake.pressed.connect(pit.shake)
	$Meter.toggled.connect(Net.show_meter)


## Called once the session runs.
func start() -> void:
	var session: NetSession = Net.session_for(pit)
	var host := session.is_server()
	$Balls.editable = host
	$Budget.editable = host
	$Trusted.disabled = not host
	$Physics.disabled = not host
	$Prediction.disabled = host
	$Shake.disabled = not host
	$Balls.set_value_no_signal(pit.ball_count)
	$Budget.set_value_no_signal(Net.budget_bytes_per_s)
	$Physics.select(BallpitSpawner.PHYSICS.keys().find(pit.ball_physics()))
	Net.show_meter($Meter.button_pressed)
	_show_values()


func _show_values() -> void:
	$BallsLabel.text = "Balls: %d" % pit.ball_count
	$BudgetLabel.text = "Budget: %d B/s per peer" % int($Budget.value)
