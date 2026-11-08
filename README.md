# Godot Project

A Godot 4.6 project.

## Develop

```
python3 tools/check.py        # build gdnet + import + validate + Rust and GDScript tests + smoke test
godot --path .                # run the game
godot --path . --editor       # open the editor
```

Godot is located via `$GODOT`, then `godot` on PATH, then `~/.cache/godot/<version>/godot`.
`bash tools/install_godot.sh` downloads the version pinned in `.godot-version`.

See `CLAUDE.md` for the test-driven workflow and project conventions.

## gdnet: multiplayer that stays close to single player

gdnet is a Rust GDExtension (`rust/gdnet`, on the sans-IO protocol engine `rust/netcore`) that makes a
single-player Godot scene multiplayer by placing it under one `NetManager` node. Everything beneath is
enrolled: scene instances become networked entities, and their transforms, physics velocities, animation
state, visibility and exported variables replicate from inferred per-class profiles, with remote copies
interpolated. Underneath are server authority, client prediction with rollback, most-recent-state deltas
under a bandwidth budget, typed calls with return values, rooms and travel between levels, and
diagnostics (a meter, network emulation, per-tick hashes, an ownership overlay).

The escalation ladder (`docs/PLAN_NETCODE.md` section 1); most games stop at rung 2:

1. **Rung 0**: put the scene under a `NetManager`; it replicates with no script changes.
2. **Rung 1**: give the player an owner and read `Net.input(self)` instead of `Input`: it is predicted.
3. **Rung 2**: call any method across the network with `Net.sync(method, args)` / `await Net.server(...)`.
4. **Rung 3**: correct the inference with `Net.configure(node, {...})` or a `NetEntity` child.
5. **Rungs 4-5**: optional hooks (`_net_tick`, `_net_spawned`, `_net_allow`, ...), then Rust.

**First run.** `python3 tools/check.py` builds the extension (`cargo build --release -p gdnet`, several
minutes the first time), imports, runs the Rust and GDScript tests and a smoke test; it ends with
`CHECK: PASS`. Then try the two demos, each a tutorial whose README is the walkthrough:

* `demos/doors/` (players travelling between levels) and `demos/ballpit/` (300 colliding `RigidBody2D`
  balls, priority and the byte budget). Open the demo's `main.tscn` and press *Run Current Scene* (F6);
  choose **Offline**, **Host** or **Join**.
* Host and clients on one machine: *Debug > Customize Run Instances...*, enable *Enable Multiple
  Instances*, set the count to 3, give the first instance the launch argument `-- --server`, run
  `main.tscn` and press **Join** in the other two windows.
* A headless dedicated server: `godot --headless --path . demos/doors/main.tscn -- --server` (likewise
  `demos/ballpit/main.tscn`).

**Where things are.** The user guide is `docs/guide/` (`01_sessions.md` to `10_physics_and_bandwidth.md`,
one page per topic). The design, work packages, decisions and status are `docs/PLAN_NETCODE.md` (its
section 10 is the status table, section 11 the known limits); the research behind it is
`docs/reports/Rust GDExtension multiplayer design.md` and `docs/research_notes/`.

**Status.** v1 is built (WP0-WP8, WP13, WP14, and the review of 2026-11-08). Not started: WP9 (lag
compensation), WP10 (a native renet peer), WP11 (the web build and demo), WP12 (an optional deterministic
mode) and WP15 (cooperative physics ownership).

**Versions.** Godot 4.6 (pinned in `.godot-version`; the extension declares `compatibility_minimum = 4.3`
and is built against gdext 0.5.5's `api-4-3`), Rust stable (developed on 1.97), gdext (`godot` crate)
0.5.5.

**Web export.** The workspace compiles for `wasm32-unknown-emscripten` (CI's `wasm-check` job; nightly Rust
and emsdk for a real export). In the browser gdnet would use Godot's own `WebRTCMultiplayerPeer` /
`WebSocketMultiplayerPeer`; this has not been exercised end to end yet (WP11).

## The `godot-project` skill

`.claude/skills/godot-project/` is a Claude Code skill that scaffolds Godot 4
projects with this same tooling (`python3 .claude/skills/godot-project/scripts/new_project.py <dir>`),
adds it to existing projects (`--adopt`), and teaches Claude the test-first loop.
Copy it to `~/.claude/skills/` to use it from any repository.
