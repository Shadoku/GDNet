# __PROJECT_NAME__

A Godot __GODOT_FEATURE__ project.

## Develop

```
python3 tools/check.py              # import + validate scripts + unit and integration tests + smoke test
python3 tools/check.py --tier quick # the fast stages (seconds), while implementing
python3 tools/check.py --filter X   # one test (a substring of `file::method`)
godot --path .                # run the game
godot --path . --editor       # open the editor
```

Godot is located via `$GODOT`, then `godot` on PATH, then `~/.cache/godot/<version>/godot`.
`bash tools/install_godot.sh` downloads the version pinned in `.godot-version`.

The stages are listed in `check.toml`; `tools/godot.py` runs the Godot side of each.
See `CLAUDE.md` for the test-driven workflow and project conventions.
