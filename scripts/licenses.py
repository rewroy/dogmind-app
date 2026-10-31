#!/usr/bin/env python3
"""Generate assets/licenses/third-party.txt: every Rust crate in the app's executable, with its license, as About shows it.

    python scripts/licenses.py

Run it after a dependency changes. The licenses of Dogmind, Straydog and Lucide are copies in assets/ beside it.
"""
import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main():
    metadata = json.loads(subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=ROOT, capture_output=True, text=True, check=True).stdout)
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    members = set(metadata["workspace_members"])
    seen, stack = set(), [member for member in members if packages[member]["name"] == "dogmind-app"]
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        for dependency in nodes[current]["deps"]:
            # What the executable links, on Windows: no test or build tools.
            kinds = dependency["dep_kinds"]
            if any(kind["kind"] is None and (kind["target"] is None or "windows" in kind["target"]) for kind in kinds):
                stack.append(dependency["pkg"])
    lines = sorted(f"{packages[id]['name']} {packages[id]['version']}  {packages[id]['license'] or 'see the crate'}" for id in seen - members)
    target = ROOT / "assets" / "licenses" / "third-party.txt"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("\n".join(lines) + "\n", encoding="utf-8", newline="\n")
    print(f"{len(lines)} crates in {target}")


if __name__ == "__main__":
    main()
