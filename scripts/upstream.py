#!/usr/bin/env python3
"""What Straydog and Dogmind have done since this app last followed them.

    python scripts/upstream.py           the commits since UPSTREAM.toml's, those touching what is watched first
    python scripts/upstream.py --diff    and the diff of the watched files
    python scripts/upstream.py --mark    after a port: move UPSTREAM.toml to where the branches are now

Run `git fetch` in each checkout first when you want what is not here yet. Nothing here changes the two projects.
"""
import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "UPSTREAM.toml"


def git(path, *args):
    done = subprocess.run(["git", "-C", str(path), *args], capture_output=True, text=True, encoding="utf-8", errors="replace")
    if done.returncode:
        sys.exit(f"git {' '.join(args)} in {path}: {done.stderr.strip()}")
    return done.stdout


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--diff", action="store_true")
    parser.add_argument("--mark", action="store_true")
    args = parser.parse_args()
    text = MANIFEST.read_text(encoding="utf-8")
    manifest = tomllib.loads(text)
    for name, entry in manifest.items():
        path = (ROOT / entry["path"]).resolve()
        span = f"{entry['commit']}..{entry['branch']}"
        if args.mark:
            head = git(path, "rev-parse", "--short", entry["branch"]).strip()
            section = re.compile(rf'(\[{name}\][^\[]*?commit = ")[^"]*(")', re.S)
            text, count = section.subn(rf"\g<1>{head}\g<2>", text)
            if count != 1:
                sys.exit(f"UPSTREAM.toml: no commit under [{name}]")
            print(f"{name}: now at {head}")
            continue
        all_commits = git(path, "log", "--oneline", span).splitlines()
        watched = git(path, "log", "--oneline", span, "--", *entry["watch"]).splitlines()
        print(f"== {name}: {len(all_commits)} commits since {entry['commit']}, {len(watched)} touch what this app follows")
        for line in watched:
            print("  " + line)
        if args.diff and watched:
            print(git(path, "diff", span, "--", *entry["watch"]))
    if args.mark:
        MANIFEST.write_text(text, encoding="utf-8", newline="\n")
        print("Now run scripts/sync_dogmind.py with Dogmind's Python, and cargo test.")


if __name__ == "__main__":
    main()
