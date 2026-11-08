extends Control
## The title screen: Host, Join or Offline. `--server` after `--` on the command line starts a
## dedicated server without a window's worth of buttons: `godot --headless --path . demos/doors/main.tscn -- --server`.

const PORT := 7350

@onready var world: DoorsWorld = $World
@onready var menu: Control = $Menu
@onready var chat: Control = $Chat


func _ready() -> void:
	$Menu/Host.pressed.connect(func() -> void: _start(Net.host(PORT)))
	$Menu/Join.pressed.connect(func() -> void: _start(Net.join($Menu/Address.text, PORT)))
	$Menu/Offline.pressed.connect(func() -> void: _start(Net.offline()))
	if "--server" in OS.get_cmdline_user_args():
		world.dedicated = true
		_start(Net.host(PORT))


func _start(error: Error) -> void:
	if error != OK:
		push_error("could not start the session: %s" % error_string(error))
		return
	menu.hide()
	chat.show()
	world.start()
	chat.start()
