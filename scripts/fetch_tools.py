#!/usr/bin/env python3
"""Fetch what goes in tools\\ beside the exe, by pinned hash, into a cache outside the repository's build output. Nothing fetched is run here.

    python -I scripts/fetch_tools.py            everything in scripts/tools.lock.json, verified, reusing what the cache has
    python -I scripts/fetch_tools.py --fonts-only   only the two fonts the app ships, and their licence, for a build run from its target folder
    python -I scripts/fetch_tools.py --relock   resolve the Python modules below again, with this machine's pip, and write their pins

The cache is %DOGMIND_TOOLS_CACHE%, or %LOCALAPPDATA%\\DogmindBuild\\tools-cache. package.py puts it together.
"""
import hashlib
import json
import os
import socket
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

LOCK = Path(__file__).resolve().parent / "tools.lock.json"
# Judged by what an agent commonly needs against size: see the sizes package.py prints.
MODULES = [
    "pip", "requests", "beautifulsoup4", "lxml", "numpy", "pandas", "matplotlib", "pillow", "pyyaml",
    "openpyxl", "python-docx", "python-pptx", "pypdf", "markdown", "jinja2", "pytest",
]


_addresses = socket.getaddrinfo


def _ipv4_first(*args, **kwargs):
    """A machine with an IPv6 address and no route for it waits out each connection otherwise."""
    return sorted(_addresses(*args, **kwargs), key=lambda found: found[0] != socket.AF_INET)


socket.getaddrinfo = _ipv4_first


def cache():
    named = os.environ.get("DOGMIND_TOOLS_CACHE")
    folder = Path(named) if named else Path(os.environ["LOCALAPPDATA"]) / "DogmindBuild" / "tools-cache"
    folder.mkdir(parents=True, exist_ok=True)
    return folder


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for block in iter(lambda: file.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def sha3_256(path):
    digest = hashlib.sha3_256()
    with open(path, "rb") as file:
        for block in iter(lambda: file.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(url, name, wanted, wanted_sha3=None):
    """The file in the cache, fetched if it is not there, and never left there unless its hash is the one pinned."""
    path = cache() / name
    if path.is_file() and sha256(path) == wanted and (wanted_sha3 is None or sha3_256(path) == wanted_sha3):
        return path
    part = path.with_name(path.name + ".part")
    request = urllib.request.Request(url, headers={"User-Agent": "dogmind-package"})
    with urllib.request.urlopen(request, timeout=120) as answer, open(part, "wb") as out:
        while block := answer.read(1 << 20):
            out.write(block)
    found = sha256(part)
    if found != wanted:
        part.unlink()
        sys.exit(f"{url}: sha256 {found}, and {wanted} is what is pinned: not used")
    if wanted_sha3 is not None:
        found_sha3 = sha3_256(part)
        if found_sha3 != wanted_sha3:
            part.unlink()
            sys.exit(f"{url}: sha3-256 {found_sha3}, and {wanted_sha3} is what is pinned: not used")
    os.replace(part, path)
    return path


def relock():
    """The modules' whole closure for the pinned Python on Windows, in wheels only: nothing of theirs is built or run."""
    lock = json.loads(LOCK.read_text(encoding="utf-8"))
    minor = ".".join(lock["python"]["version"].split(".")[:2])
    with tempfile.TemporaryDirectory() as folder:
        report = Path(folder) / "report.json"
        subprocess.run(
            [sys.executable, "-I", "-m", "pip", "install", "--dry-run", "--ignore-installed", "--target", str(Path(folder) / "t"), "--report", str(report), "--quiet",
             "--platform", "win_amd64", "--python-version", minor, "--implementation", "cp", "--only-binary=:all:", *MODULES],
            check=True,
        )
        installed = json.loads(report.read_text(encoding="utf-8"))["install"]
    wheels = []
    for item in installed:
        url = item["download_info"]["url"]
        wheels.append({"name": item["metadata"]["name"], "version": item["metadata"]["version"], "file": url.rsplit("/", 1)[1], "url": url, "sha256": item["download_info"]["archive_info"]["hashes"]["sha256"]})
    lock["wheels"] = sorted(wheels, key=lambda wheel: wheel["name"].lower())
    LOCK.write_text(json.dumps(lock, indent=1) + "\n", encoding="utf-8", newline="\n")
    print(f"{len(wheels)} wheels pinned for Python {minor}")


def main():
    if "--relock" in sys.argv:
        relock()
    lock = json.loads(LOCK.read_text(encoding="utf-8"))
    for key in () if "--fonts-only" in sys.argv else ("python", "sqlite", "node", "git"):
        item = lock[key]
        print(key, item["version"], fetch(item["url"], item["file"], item["sha256"], item.get("sha3_256")))
    for item in lock["fonts"]["files"] + [lock["fonts"]["license"]]:
        fetch(item["url"], item["file"], item["sha256"])
    if "--fonts-only" in sys.argv:
        print(f"fonts in {cache()}")
        return
    for wheel in lock["wheels"]:
        fetch(wheel["url"], wheel["file"], wheel["sha256"])
    print(f"{len(lock['wheels'])} wheels in {cache()}")


if __name__ == "__main__":
    main()
