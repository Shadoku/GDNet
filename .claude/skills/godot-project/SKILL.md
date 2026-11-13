---
name: godot-project
description: Create, set up, and develop Godot 4+ projects with a headless test loop so Claude can verify its own changes instead of asking the user to run the game. Use this whenever the user wants to start a new Godot project, add tests/CI/AI tooling to an existing Godot project, run or write GDScript unit tests, fix a failing check, or asks how to verify Godot code works. Trigger on "new godot project", "scaffold", "set up tests for my game", "godot CI", "run the tests", "headless godot", "check.py", "TestCase", or any Godot 4 task where the outcome should be verified automatically, even if the user never says "test".
---

# Godot 4 project kit

This skill gives every Godot project the same verifiable loop: `python3 tools/check.py`
imports the project, compiles every script, runs unit and integration tests, and boots
the main scene headless, then ends with `CHECK: PASS` or `CHECK: FAIL`. Nothing needs an
addon or the editor, so it works in CI and in Claude Code on the web.

`tools/check.py` is the language-agnostic check of the `tdd-project` kit: it runs the
stages listed in `check.toml` and adds tiers (`--tier quick` for the inner loop,
`--filter <name>` for one test), a content-keyed pass cache (`[CACHED]`), parallel stages
and a detector for stages that mostly wait. The Godot work (finding the binary, judging
Godot's error output, the test runner) is in `tools/godot.py`, one subcommand per stage.
`check.toml`'s keys are documented in its head comment and in the tdd-project skill's
`references/check-toml.md`.

For GDScript style, node architecture, or Tiled/YATI questions, also consult the
`godot-gd` skill (it may be listed under a prefix such as `anthropic-skills:godot-gd`)
if it is available; this skill is about project setup and verification.

## Decide which mode applies

| Situation | Do |
|---|---|
| No `project.godot` yet, or user asks for a new project | **Scaffold** (below) |
| `project.godot` exists but no `tools/check.py` | **Adopt** the kit into it |
| `tools/check.py` exists | **Develop** with the test loop |

Check with `ls project.godot tools/check.py tests/framework` before choosing.

## Scaffold a new project

1. Settle four inputs, defaulting rather than asking when the user did not say:
   name (folder name), `--type 2d|3d` (2d), renderer (gl_compatibility for 2d,
   forward_plus for 3d; gl_compatibility is the safe choice for web export),
   Godot version (4.6 unless the user has another installed: `godot --version`).
   Genre words such as "top-down", "platformer", or "roguelike" change nothing
   here; they shape the first features you write afterwards.
2. Run the scaffolder from this skill's directory:
   ```
   python3 <skill-dir>/scripts/new_project.py <target> --name "Name" --type 2d --godot-version 4.6
   ```
   It writes project.godot, a main scene, an example logic class with tests, the
   test framework, `tools/`, CI, the Claude session hook, and `CLAUDE.md`, copies
   this skill into `<target>/.claude/skills/` so future sessions have it, and
   runs `git init` if the target is not already inside a repository. Several of
   the files are dotfiles (`.github/`, `.claude/`, `.godot-version`, ...): use
   `ls -la` when checking what was written.
3. Make sure a Godot binary is reachable. `tools/godot.py` looks at `$GODOT`, then
   `godot`/`godot4` on PATH, then `~/.cache/godot/<version>/godot`. If none
   exists, `bash tools/install_godot.sh` downloads the pinned release into that
   cache (Linux/macOS); it is idempotent and reuses a cached binary, so running
   it is always safe.
4. Run `python3 tools/check.py` in the new project. It must end with `CHECK: PASS`
   before you report success. The first `--import` creates `*.uid` and `*.import`
   sidecar files; commit them with the rest (until they are committed or ignored the
   check prints `note: this pass was not stored ...`). A second run on unchanged files
   must print `[CACHED]`.
5. Replace the example `Health` class and its test with the user's first real
   feature when they describe one; otherwise leave them as a worked example.

## Adopt the kit into an existing project

```
python3 <skill-dir>/scripts/new_project.py <project-dir> --adopt
```
This adds only missing files and never touches project.godot, scenes, or
scripts. Then run `python3 tools/check.py`. Expect the validate stage to surface
pre-existing script errors; report them to the user rather than silently fixing
unrelated code. If `CLAUDE.md` already exists, merge the Commands, development
loop, "How the check stays fast" (with Subagents), "Reading the report" and Writing
tests sections from `assets/template/CLAUDE.md` into it. If the project has more to
verify (a native build, a linter), add a stage to `check.toml` rather than running it
by hand. If the project already uses GUT or gdUnit4, keep that framework and only take
`tools/check.py`, `tools/godot.py`, `check.toml`, `validate_scripts.gd`, and CI; see
`references/testing.md`.

## Develop with the loop

The point of the kit is that you verify instead of asking the user to try it.

1. Write or adjust a test under `tests/` first. It documents the intent and
   proves the failure exists (`python3 tools/check.py --filter <name>`; after adding
   a file or a `class_name`, `--tier quick` once first, since `--filter` skips import).
2. Implement. Put rules and state in plain `RefCounted`/`Resource` classes, keep
   node scripts thin, so logic is testable in milliseconds without a scene.
   While implementing, re-run `--filter` and `--tier quick`, not the full check.
3. Run `python3 tools/check.py` once when done. Read failures top-down; the earliest
   stage explains later ones. `--only <stage> --verbose` shows raw engine output when
   a summary is not enough.
4. Repeat until `CHECK: PASS`, then report with the `TESTS:` lines quoted. Work a
   subagent returns is verified by running the check yourself without `--fresh`:
   unchanged files answer from the pass cache.

What the check catches that reasoning alone misses: parse and static-type
errors, wrong `class_name` or node paths, `push_error` and null access at
runtime (any error printed during a test fails that test), tests that await
forever (per-test timeout), and a main scene that crashes on boot.

Common failure signatures and the usual cause are tabulated in the project's
`CLAUDE.md`; `references/testing.md` has the full `TestCase` API and patterns
for scenes, signals, input, physics, and autoloads.

## Reference files

- `references/testing.md`: TestCase API, test patterns, what headless can and
  cannot test, switching to GUT/gdUnit4. Read before writing non-trivial tests.
- `references/headless-cli.md`: Godot CLI flags, how errors and exit codes
  behave, the `.godot/` cache and `*.uid` files. Read when running Godot by hand
  or when a check behaves unexpectedly.
- `references/project-layout.md`: folder conventions, project.godot settings
  worth setting early (autoloads, input map, physics layers), git hygiene, export.
  Read when structuring a project or adding autoloads/input actions.

## Bundled files

- `scripts/new_project.py`: the scaffolder (new project or `--adopt`).
- `assets/template/`: every file the scaffolder writes. `tests/framework/runner.gd`
  and `test_case.gd` are the test framework (`--dir` limits discovery to one folder);
  `tools/check.py` the check command (the tdd-project kit's, verbatim), `check.toml`
  its stages, `tools/godot.py` the Godot side of each stage;
  `tools/install_godot.sh` the pinned download used by CI and the session hook.

Keep the template and a scaffolded project in sync: when you improve the runner,
`tools/godot.py` or `check.toml` inside a project, port the change back to
`assets/template/`; a fix to `tools/check.py` goes to the tdd-project kit's template
as well, so both kits keep one copy of it.
