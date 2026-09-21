# Godot Project

A Godot 4.6 project.

## Develop

```
python3 tools/check.py        # import + validate scripts + unit tests + smoke test
godot --path .                # run the game
godot --path . --editor       # open the editor
```

Godot is located via `$GODOT`, then `godot` on PATH, then `~/.cache/godot/<version>/godot`.
`bash tools/install_godot.sh` downloads the version pinned in `.godot-version`.

See `CLAUDE.md` for the test-driven workflow and project conventions.
