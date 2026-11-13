extends Control
## The title screen is gdnet's connect menu (`addons/gdnet/connect/`): Host, Host on Steam, Join (an
## address or a join code) or Offline. `--server` after `--` on the command line starts a dedicated server
## without a window's worth of buttons: `godot --headless --path . demos/doors/main.tscn -- --server`.

@onready var world: DoorsWorld = $World
@onready var menu: NetConnectMenu = $Menu
@onready var chat: Control = $Chat


func _ready() -> void:
	menu.started.connect(_start)
	if "--server" in OS.get_cmdline_user_args():
		world.dedicated = true
		menu.host()


func _start() -> void:
	chat.show()
	world.start()
	chat.start()
