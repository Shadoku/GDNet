# Godot 4 project layout and settings

## Contents
1. Folder conventions
2. project.godot settings to set early
3. Scenes and scripts
4. Git hygiene
5. Export

## 1. Folder conventions

```
project.godot
.godot-version              pinned release for tools/install_godot.sh and CI
scenes/<feature>/           player/player.tscn, enemies/slime.tscn, ui/hud.tscn
src/<feature>/              scripts for those scenes plus plain logic classes
src/autoload/               autoload singletons (event bus, game state, settings)
data/                       .tres resources (stats, levels, dialogue) and their Resource scripts
assets/                     art, audio, fonts (imported by Godot; .import files live beside them)
addons/                     third-party plugins; excluded from validate_scripts by default
tests/unit/ tests/integration/ tests/framework/
tools/                      check.py, validate_scripts.gd, install_godot.sh, other headless tools
```

Keep a scene and its script in matching folders rather than one giant `scripts/`
folder; the tree then mirrors the game's features. Small projects can start flat
(`scenes/`, `src/`) and split by feature once a folder passes about ten files.

## 2. project.godot settings to set early

Edit in the editor when possible; these are the sections you will see and can
also add by hand (keep section headers alphabetical, Godot rewrites the file).

```ini
[autoload]
GameState="*res://src/autoload/game_state.gd"   ; leading * = singleton node

[input]
jump={ "deadzone": 0.5, "events": [Object(InputEventKey,"resource_local_to_scene":false,"resource_name":"","device":-1,"window_id":0,"alt_pressed":false,"shift_pressed":false,"ctrl_pressed":false,"meta_pressed":false,"pressed":false,"keycode":0,"physical_keycode":32,"key_label":0,"unicode":32,"location":0,"echo":false,"script":null)] }

[layer_names]
2d_physics/layer_1="world"
2d_physics/layer_2="player"
2d_physics/layer_3="enemies"

[display]
window/size/viewport_width=640
window/size/viewport_height=360
window/stretch/mode="canvas_items"   ; pixel art: also rendering/textures/canvas_textures/default_texture_filter=0

[rendering]
renderer/rendering_method="gl_compatibility"   ; 2D and web-friendly; forward_plus for 3D desktop
```

Input events written by hand are error-prone; add actions in the editor
(Project Settings > Input Map) and commit the result. Name physics layers before
using them so `collision_layer` values in scenes stay readable.

Autoloads named in project.godot are available in headless test runs too.

## 3. Scenes and scripts

- One script per scene root; the script's `class_name` matches the scene
  (`Player` for `player.tscn`).
- `@export` what a designer configures; `@onready var x: Type = $Path` for
  child references; `%UniqueName` for nodes marked unique to survive reparenting.
- Emit signals upward, call methods downward, never reach across siblings.
- Logic that does not need the tree (rules, stats, inventories, state machines)
  goes in `RefCounted`/`Resource` classes so tests can create it with `.new()`.
- Resource subclasses in `data/` with `@export` fields give designers editable
  `.tres` files and give tests plain objects.

## 4. Git hygiene

Ignore `.godot/`, exported builds, and `junit.xml`. Commit `*.import`, `*.uid`,
`export_presets.cfg` (unless it holds signing secrets: keep those in
environment variables and use `--export-release` with them set). Normalize line
endings with `.gitattributes` so Godot's LF-only files do not churn on Windows.

Large binary assets: consider Git LFS for anything over a few MB
(`git lfs track "*.png" "*.ogg"`).

## 5. Export

Export presets live in `export_presets.cfg`, created from the editor. Headless
export needs export templates for the same version: download from
`https://github.com/godotengine/godot/releases/download/<ver>/Godot_v<ver>_export_templates.tpz`
and unpack into `~/.local/share/godot/export_templates/<ver>/`. Then
`godot --headless --path . --export-release "<preset name>" build/out`.
Wire it into CI as a separate job only once the game has something to ship.
