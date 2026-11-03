#!/usr/bin/env python3
"""Generate assets/licenses/third-party.txt: every Rust crate in what is shipped, the app's executable, its installer and straydog.exe, with its license and the license's own words.

    python scripts/licenses.py [path to Straydog's source]

Run it after a dependency changes, here or in Straydog. MIT, BSD and the like ask that their text go with a binary, so a list of names is not enough. The same words are given once,
with the crates they cover. The licenses of Dogmind, Straydog and Lucide are copies in assets/ beside it. It reads what cargo has already fetched, and downloads nothing.
"""
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET = "x86_64-pc-windows-msvc"
LICENSE_FILES = ("license", "licence", "copying", "unlicense", "notice", "copyright")


def crates(workspace, root_package, options=()):
    """The packages `root_package` links on Windows: no test or build tools, as `--filter-platform` has dropped what is for other systems."""
    command = ["cargo", "metadata", "--format-version", "1", "--locked", "--offline", "--filter-platform", TARGET, *options]
    metadata = json.loads(subprocess.run(command, cwd=workspace, capture_output=True, text=True, check=True).stdout)
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    members = set(metadata["workspace_members"])
    seen, stack = set(), [member for member in members if packages[member]["name"] == root_package]
    if not stack:
        sys.exit(f"no package {root_package} in {workspace}")
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        for dependency in nodes[current]["deps"]:
            if any(kind["kind"] is None for kind in dependency["dep_kinds"]):
                stack.append(dependency["pkg"])
    return [packages[id] for id in seen - members]


def license_texts(package):
    """The words of a crate's license files, as it ships them."""
    folder = Path(package["manifest_path"]).parent
    texts = []
    for file in sorted(folder.iterdir()):
        if file.is_file() and file.name.lower().startswith(LICENSE_FILES) and file.suffix.lower() not in (".rs", ".toml", ".py", ".sh"):
            try:
                text = file.read_text(encoding="utf-8").strip()
            except UnicodeDecodeError:
                text = file.read_text(encoding="latin-1").strip()
            if text:
                texts.append(text)
    return texts


def main():
    straydog = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT.parent / "Straydog"
    shipped = {}
    for package in crates(ROOT, "dogmind-app") + crates(ROOT, "dogmind-setup") + crates(straydog, "straydog", ["--features", "straydog/cuda"]):
        shipped[(package["name"], package["version"])] = package
    lines, by_text, without = [], {}, []
    for (name, version), package in sorted(shipped.items()):
        label = f"{name} {version}"
        lines.append(f"{label}  {package['license'] or 'see the crate'}")
        texts = license_texts(package)
        if not texts:
            authors = ", ".join(package.get("authors") or [])
            without.append(f"{label}  {package['license'] or 'see the crate'}" + (f"  (c) {authors}" if authors else ""))
        for text in texts:
            by_text.setdefault(text, []).append(label)
    target = ROOT / "assets" / "licenses" / "third-party.txt"
    target.parent.mkdir(parents=True, exist_ok=True)
    parts = ["\n".join(lines), ""]
    for text, names in sorted(by_text.items(), key=lambda item: (item[1][0], item[0])):
        parts += ["-" * 72, "These crates: " + ", ".join(names), "-" * 72, text, ""]
    if without:
        parts += ["-" * 72, "These crates ship no license file, and are under the license named, in its standard words:", "\n".join(without), ""]
    target.write_text("\n".join(parts) + "\n", encoding="utf-8", newline="\n")
    print(f"{len(lines)} crates, {len(by_text)} license texts, {len(without)} crates with no file, in {target}")


if __name__ == "__main__":
    main()
