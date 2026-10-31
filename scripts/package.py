#!/usr/bin/env python3
"""Put together what is shipped: dist/DogmindApp with the app, straydog.exe and one LICENSE.txt.

    cargo build --release -p dogmind-app
    python scripts/licenses.py
    python scripts/package.py [path to straydog.exe]

LICENSE.txt holds this app's license, those of Dogmind, Straydog and Lucide, and the list of Rust crates in the executable.
"""
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PARTS = [
    ("Dogmind App", ROOT / "LICENSE", None),
    ("Dogmind", ROOT / "assets" / "licenses" / "dogmind-LICENSE.txt", "The system prompt and the tool descriptions come from Dogmind, and its agent's behaviour follows it."),
    ("Straydog", ROOT / "assets" / "licenses" / "straydog-LICENSE.txt", "The engine, a separate program beside this one."),
    ("Lucide", ROOT / "assets" / "lucide" / "LICENSE", "The icons."),
    ("Rust crates in this executable", ROOT / "assets" / "licenses" / "third-party.txt", None),
]


def main():
    target = ROOT / "dist" / "DogmindApp"
    target.mkdir(parents=True, exist_ok=True)
    shutil.copy2(ROOT / "target" / "release" / "dogmind-app.exe", target / "dogmind-app.exe")
    straydog = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT.parent / "Straydog" / "target" / "release" / "straydog.exe"
    if straydog.is_file():
        shutil.copy2(straydog, target / "straydog.exe")
    else:
        print(f"no straydog.exe at {straydog}: the app will ask for one", file=sys.stderr)
    text = []
    for title, path, note in PARTS:
        text.append("=" * 72 + f"\n{title}\n" + (f"{note}\n" if note else "") + "=" * 72 + "\n\n" + path.read_text(encoding="utf-8").strip() + "\n\n")
    (target / "LICENSE.txt").write_text("".join(text), encoding="utf-8", newline="\r\n")
    print(f"{target}: " + ", ".join(sorted(file.name for file in target.iterdir())))


if __name__ == "__main__":
    main()
