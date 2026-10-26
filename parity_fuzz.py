"""Randomized parity of sureforge_gix.stage_and_write with `git add -A` + `git write-tree`.

usage: python3 fingerprint/parity_fuzz.py [iterations-per-category] [seed]

Each category starts from a random committed tree, applies random work-tree
edits, then stages both ways: git into a scratch index and a private object
dir, the module into its own store. Prints one line per category and exits 1
on any mismatched tree, naming the seed that reproduces it. An error is not a
mismatch: the engine reruns that call on git, so it only costs speed; such
fallbacks are counted apart from deliberate `route:` refusals.
"""

import os
import random
import shutil
import subprocess
import sys
import tempfile
import unittest.mock
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "runtime"))
import sureforge_gix as gix  # noqa: E402
import sureforge_hook as hook  # noqa: E402

NAMES = ["a", "a.txt", "a-", "a0", "a.b", "b", "B", "x y", "ü.txt", "z", "dir", "dir.txt", "dir-"]
CATEGORIES = {
    "plain": {},
    "text-auto": {"attributes": "* text=auto\n"},
    "autocrlf-true": {"config": [("core.autocrlf", "true")]},
    "eol-attributes": {"attributes": "*.txt eol=lf\n*.b eol=crlf\n*.dat binary\n", "config": [("core.autocrlf", "input")]},
    "gitignore": {"gitignore": True},
    "staged": {"stage": True},
    "untracked-hidden": {"config": [("status.showUntrackedFiles", "no")]},
    "symlinks-off": {"config": [("core.symlinks", "false")]},
    # Whole fingerprint, both engines: tracked rules with negations plus outside excludes.
    "gitignore-outside": {"outside": True},
}
TRACKED_RULES = ["*.b", "!keep.b", "dir/", "!dir/a", "!a", "a0", "x y/", "*.dat", "!z", "dir/*", "!dir/a.txt", "!*.b", "!a?", "!dir/a0", "!dir/dir", "!dir/\u00fc.txt"]
OUTSIDE_RULES = ["*.b", "keep.b", "dir/", "a", "a.txt", "**/z", "B", "x y/", "dir/a", "*.txt", "a0/", "dir", "x y", "dir/x y",
                 "dir/**/a", "**/dir/a", "dir/**", "dir/a0/*.txt", "?????", "a?", "*.txt "]


def git(root, *args, env=None):
    proc = subprocess.run(["git", "-C", str(root), *args], capture_output=True, env=env)
    if proc.returncode:
        raise RuntimeError(f"git {' '.join(args)}: {proc.stderr.decode(errors='replace')}")
    return proc.stdout.decode().rstrip()  # leading whitespace is data: " M a.txt" starts with a space


def changed_paths(root):
    return [line[3:] for line in git(root, "-c", "core.quotePath=false", "status", "--porcelain", "-uall").splitlines()]


def tracked_paths(root):
    return [line for line in git(root, "-c", "core.quotePath=false", "ls-files").splitlines() if line]


def content(rng):
    kind = rng.random()
    if kind < 0.15:
        return bytes(rng.randrange(256) for _ in range(rng.randrange(1, 40)))
    lines = [rng.choice(["x", "y", "line", ""]) * rng.randrange(1, 4) for _ in range(rng.randrange(0, 5))]
    eol = rng.choice([b"\n", b"\r\n", b"\n", b"\r\n", b"\r"])
    body = eol.join(line.encode() for line in lines)
    return body + (eol if rng.random() < 0.7 else b"")


def rel(rng, depth=0):
    name = rng.choice(NAMES)
    return name if depth > 1 or rng.random() < 0.5 else f"{name}/{rel(rng, depth + 1)}"


def place(root, path, rng, mode="file"):
    full = root / path
    try:
        full.parent.mkdir(parents=True, exist_ok=True)
        if full.is_symlink() or full.is_file():
            full.unlink()
        elif full.is_dir():
            shutil.rmtree(full)
        if mode == "link":
            os.symlink(rng.choice(["a", "../a.txt", "dir", "missing", "."]), full)
        else:
            full.write_bytes(content(rng))
            if rng.random() < 0.2:
                full.chmod(0o755)
    except (OSError, NotADirectoryError):
        pass  # a parent is a file or a link: skip this edit


def build(root, rng, spec):
    root.mkdir()
    git(root, "init", "-q")
    for key, value in spec.get("config", []):
        git(root, "config", key, value)
    if "attributes" in spec:
        (root / ".gitattributes").write_text(spec["attributes"])
    if spec.get("outside"):
        (root / ".gitignore").write_text("\n".join(rng.sample(TRACKED_RULES, rng.randrange(1, 6))) + "\n")
        info = root / ".git" / "info" / "exclude"
        info.parent.mkdir(exist_ok=True)
        info.write_text("\n".join(rng.sample(OUTSIDE_RULES, rng.randrange(0, 4))) + "\n")
        if rng.random() < 0.5:
            other = Path(os.environ["HOME"]) / f"ignore-{rng.randrange(10**9)}"
            other.write_text("\n".join(rng.sample(OUTSIDE_RULES, rng.randrange(1, 4))) + "\n")
            git(root, "config", "core.excludesFile", str(other))
    if spec.get("gitignore"):
        rules = rng.sample(["*.b", "!keep.b", "dir/", "a0", "x y/", "*.dat", "!dir/a"], rng.randrange(1, 5))
        (root / ".gitignore").write_text("\n".join(rules) + "\n")
    for _ in range(rng.randrange(3, 15)):
        place(root, rel(rng), rng, "link" if rng.random() < 0.1 else "file")
    git(root, "add", "-A")
    git(root, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "base", "--allow-empty")
    if spec.get("outside") and rng.random() < 0.3:
        tracked = tracked_paths(root)
        if len({path.lower() for path in tracked}) == len(tracked):  # `b` and `B` cannot both exist where it is true
            git(root, "config", "core.ignoreCase", "true")


def mutate(root, rng, spec):
    tracked = tracked_paths(root)
    for _ in range(rng.randrange(1, 10)):
        action = rng.random()
        if tracked and action < 0.3:
            target = root / rng.choice(tracked)
            if target.is_file() and not target.is_symlink():
                target.write_bytes(content(rng)) if rng.random() < 0.7 else target.chmod(target.stat().st_mode ^ 0o111)
        elif tracked and action < 0.45:
            target = root / rng.choice(tracked)
            if target.is_symlink() or target.is_file():
                target.unlink()
        elif action < 0.55:
            place(root, rel(rng), rng, "link")
        else:
            place(root, rel(rng), rng)
    if spec.get("stage"):
        changed = changed_paths(root)
        for path in rng.sample(changed, k=min(len(changed), rng.randrange(0, 3))):
            flag = ["-N"] if rng.random() < 0.3 else []
            try:
                git(root, "add", *flag, "--", path.strip('"'))
            except RuntimeError:
                pass


def cli_tree(root, tmp):
    index = Path(tmp) / "index"
    real = root / ".git" / "index"
    if real.exists():
        shutil.copy2(real, index)
    env = {**os.environ, "GIT_INDEX_FILE": str(index), "GIT_OBJECT_DIRECTORY": str(Path(tmp) / "cli-objects"),
           "GIT_ALTERNATE_OBJECT_DIRECTORIES": str(root / ".git" / "objects")}
    Path(env["GIT_OBJECT_DIRECTORY"]).mkdir()
    git(root, "-c", "core.splitIndex=false", "add", "-A", env=env)
    return git(root, "-c", "core.splitIndex=false", "write-tree", "--missing-ok", env=env)


def compare_engines(root):
    """'same', 'routed', 'error: ...', or 'cli answer|other answer' for the whole fingerprint.

    The CLI answer is first checked against git with the outside-exclude prefilter off: a
    prefilter that skips a source git would list from is unsound.
    """
    top = str(root)
    cli_errors, oracle_errors = [], []
    expected = (hook._git_fingerprint(top, None, cli_errors, hook._CLI), cli_errors)
    with unittest.mock.patch.object(hook, "_exclude_might_match", lambda *_, **__: True):
        oracle = (hook._git_fingerprint(top, None, oracle_errors, hook._CLI), oracle_errors)
    if oracle != expected:
        return f"{oracle}|prefilter {expected}"
    if gix.preflight(top, hook.INPROCESS_MAX_ENTRIES)[0] != "inprocess":
        return "routed"
    gix_errors = []
    try:
        got = (hook._git_fingerprint(top, None, gix_errors, hook._InProcess(gix)), gix_errors)
    except RuntimeError as exc:
        return f"error: {exc}"
    return "same" if got == expected else f"{expected}|{got}"


def run(category, spec, iterations, seed):
    failures, routed, fell_back = [], 0, []
    for i in range(iterations):
        case = f"{seed}:{category}:{i}"
        rng = random.Random(case)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "r"
            build(root, rng, spec)
            mutate(root, rng, spec)
            if spec.get("outside"):
                outcome = compare_engines(root)
                if outcome == "routed":
                    routed += 1
                elif outcome.startswith("error: "):
                    fell_back.append((case, outcome[7:]))
                elif outcome != "same":
                    failures.append((case, *outcome.split("|", 1)))
                continue
            expected = cli_tree(root, tmp)
            try:
                got = gix.stage_and_write(str(root), str(Path(tmp) / "gix-objects"), [], [])
            except RuntimeError as exc:
                got = f"error: {exc}"
            if got.startswith("error: route:"):
                routed += 1  # the engine hands this call to git, whose answer is `expected` by definition
            elif got.startswith("error: "):
                fell_back.append((case, got[7:]))
            elif got != expected:
                failures.append((case, expected, got))
    return failures, routed, fell_back


def main():
    iterations = int(sys.argv[1]) if len(sys.argv) > 1 else 1000
    seed = sys.argv[2] if len(sys.argv) > 2 else "f0"
    env = {"HOME": tempfile.mkdtemp(), "GIT_CONFIG_NOSYSTEM": "1"}
    os.environ.update(env)
    bad = 0
    for category, spec in CATEGORIES.items():
        failures, routed, fell_back = run(category, spec, iterations, seed)
        bad += len(failures)
        status = "pass" if not failures else "FAIL"
        print(f"{category:15} {status} {iterations - len(failures)}/{iterations} (routed to git: {routed},"
              f" fell back to git: {len(fell_back)}{' e.g. ' + fell_back[0][0] + ' ' + fell_back[0][1] if fell_back else ''})"
              + (f"  first: {failures[0][0]} git={failures[0][1]} gix={failures[0][2]}" if failures else ""))
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main())
