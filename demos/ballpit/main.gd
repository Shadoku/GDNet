extends Control
## The title screen is gdnet's connect menu (`addons/gdnet/connect/`): Host, Host on Steam, Join (an
## address or a join code) or Offline. `--server` after `--` on the command line starts a dedicated
## server: `godot --headless --path . demos/ballpit/main.tscn -- --server`.

@onready var pit: BallpitSpawner = $Pit
@onready var menu: NetConnectMenu = $Menu
@onready var hud: BallpitHud = $Hud


func _ready() -> void:
	menu.started.connect(_start)
	if "--server" in OS.get_cmdline_user_args():
		pit.dedicated = true
		menu.host()


func _start() -> void:
	hud.show()
	pit.start()
	hud.start()
