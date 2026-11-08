#!/usr/bin/env python3
"""Run every automated check for this Godot project and print a compact report.

Stages (in order):
  import    - (re)import assets and rebuild the class_name cache  (godot --import)
  validate  - compile every .gd script and report parse/type errors
  test      - run the unit tests under tests/ with tests/framework/runner.gd
  smoke     - run the main scene headless for a few frames and fail on any error

Usage:
  python3 tools/check.py                # everything
  python3 tools/check.py --only test    # one stage (import always runs first)
  python3 tools/check.py --only test --no-import   # fastest loop when no files were added
  python3 tools/check.py --filter=player  # pass a test filter to the runner
  python3 tools/check.py --verbose      # show full engine output for every stage
  python3 tools/check.py --jobs 1       # run the test files in one Godot process (default 2)

Exit code is 0 only when every stage passed. The last line is always
"CHECK: PASS" or "CHECK: FAIL (<stages>)" so the result is easy to spot.

Environment:
  GODOT   - path to the Godot 4 binary. If unset, PATH (godot, godot4) and
            ~/.cache/godot/<version>/godot are tried, where <version> comes from
            the .godot-version file next to project.godot.
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SMOKE_FRAMES = 30
STAGE_TIMEOUT = {"import": 300, "validate": 120, "test": 600, "smoke": 120}

# Error lines matching any of these regexes are ignored (known-benign headless noise).
IGNORE_PATTERNS = [
    r"XDG_RUNTIME_DIR",
    r"ALSA lib",
]

ERROR_PREFIX = re.compile(r"^(SCRIPT ERROR|ERROR|USER ERROR):")
BANNER = re.compile(r"^Godot Engine v\d")


def godot_version() -> str:
    f = ROOT / ".godot-version"
    return f.read_text().strip() if f.exists() else "4.6-stable"


def find_godot() -> str:
    candidates = []
    if os.environ.get("GODOT"):
        candidates.append(os.environ["GODOT"])
    for name in ("godot", "godot4", "godot4-stable"):
        p = shutil.which(name)
        if p:
            candidates.append(p)
    candidates.append(str(Path.home() / ".cache" / "godot" / godot_version() / "godot"))
    candidates.append(str(Path.home() / ".cache" / "godot" / "godot"))
    for c in candidates:
        if os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    sys.exit(
        "No Godot binary found. Set GODOT=/path/to/godot, put `godot` on PATH, or run "
        ".claude/hooks/session-start.sh to download " + godot_version() + "."
    )


def run(godot: str, args: list[str], timeout: int) -> tuple[int, str]:
    cmd = [godot, "--headless", "--path", str(ROOT), *args]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, cwd=ROOT)
        out = proc.stdout + proc.stderr
        return proc.returncode, out
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or "") + (e.stderr or "")
        if isinstance(out, bytes):
            out = out.decode(errors="replace")
        return 124, out + f"\n[check.py] TIMEOUT after {timeout}s: {' '.join(cmd)}\n"


def error_blocks(output: str) -> list[str]:
    """Extract ERROR / SCRIPT ERROR blocks (first line + indented continuation lines)."""
    blocks: list[list[str]] = []
    for line in output.splitlines():
        if ERROR_PREFIX.match(line):
            blocks.append([line])
        elif blocks and line[:1] in (" ", "\t") and blocks[-1] is not None:
            blocks[-1].append(line)
        else:
            blocks.append(None)  # break continuation
    result = []
    for b in blocks:
        if b is None:
            continue
        # Backtrace frames inside our own tooling are noise for the reader.
        b = [l for l in b if not ("validate_scripts.gd" in l and l.strip().startswith("["))]
        text = "\n".join(b)
        if any(re.search(p, text) for p in IGNORE_PATTERNS):
            continue
        result.append(text)
    return result


def clean(output: str) -> str:
    return "\n".join(l for l in output.splitlines() if not BANNER.match(l)).strip()


def default_jobs() -> int:
    """Two test processes when there are two CPUs to run them: the tests mostly wait on the clock."""
    return 2 if (os.cpu_count() or 1) >= 2 else 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--only", choices=["import", "validate", "test", "smoke"], help="run a single stage (import always runs first)")
    ap.add_argument("--no-import", action="store_true", help="skip the import stage (safe only when no scripts, scenes or assets were added or renamed)")
    ap.add_argument("--filter", help="only run tests whose 'file::method' id contains this substring")
    ap.add_argument("--verbose", action="store_true", help="print full engine output for every stage")
    ap.add_argument("--junit", help="write JUnit XML for the test stage to this path")
    ap.add_argument("--jobs", type=int, default=default_jobs(), help="test files run in this many Godot processes at once (default: 2 with 2+ CPUs; 1 = one process, the old way)")
    ap.add_argument("--smoke-frames", type=int, default=SMOKE_FRAMES)
    ap.add_argument("--skip-smoke", action="store_true")
    ap.add_argument("--include-addons", action="store_true", help="also validate scripts under addons/")
    args = ap.parse_args()

    godot = find_godot()
    print(f"godot: {godot}")
    failed: list[str] = []

    def stage(name: str, godot_args: list[str], judge) -> None:
        if args.only and name != "import" and args.only != name:
            return
        t0 = time.time()
        code, out = run(godot, godot_args, STAGE_TIMEOUT[name])
        ok, details = judge(code, out)
        dt = time.time() - t0
        print(f"[{'PASS' if ok else 'FAIL'}] {name} ({dt:.1f}s)")
        if args.verbose:
            print("    " + clean(out).replace("\n", "\n    "))
        elif not ok:
            print("    " + details.replace("\n", "\n    "))
        else:
            # Always show the one-line summary so a report can quote it.
            for line in out.splitlines():
                if line.startswith(("TESTS:", "SCRIPTS:")):
                    print("    " + line)
        if not ok:
            failed.append(name)

    # 1. import: rebuilds .godot/, imports assets, refreshes class_name cache.
    if not args.no_import:
        stage("import", ["--import"], lambda code, out: (code == 0, clean(out)[-3000:]))

    # 2. validate: every script compiles. Details = the SCRIPT ERROR lines.
    def judge_validate(code, out):
        blocks = error_blocks(out)
        summary = [l for l in out.splitlines() if l.startswith(("[FAIL]", "SCRIPTS:"))]
        return code == 0, "\n".join(blocks + summary) or clean(out)[-3000:]
    validate_args = ["-s", "res://tools/validate_scripts.gd"]
    if args.include_addons:
        validate_args += ["--", "--include-addons"]
    stage("validate", validate_args, judge_validate)

    # 3. test: the runner already prints only failures + summary; show that verbatim.
    test_args = ["-s", "res://tests/framework/runner.gd", "--"]
    if args.verbose:
        test_args.append("--verbose")
    if args.filter:
        test_args.append(f"--filter={args.filter}")
    if args.junit:
        test_args.append(f"--junit={os.path.abspath(args.junit)}")
    if args.jobs > 1:
        test_args.append(f"--jobs={args.jobs}")
    stage("test", test_args, lambda code, out: (code == 0, clean(out)))

    # 4. smoke: main scene boots and runs N frames without printing any error.
    if not args.skip_smoke:
        def judge_smoke(code, out):
            blocks = error_blocks(out)
            ok = code == 0 and not blocks
            details = "\n".join(blocks) if blocks else clean(out)[-3000:]
            if code != 0:
                details = f"exit code {code}\n" + details
            return ok, details
        stage("smoke", ["--quit-after", str(args.smoke_frames)], judge_smoke)

    print()
    if failed:
        print(f"CHECK: FAIL ({', '.join(failed)})")
        return 1
    print("CHECK: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
