#!/usr/bin/env python3
"""Put together what is shipped: dist/DogmindApp with the app, straydog.exe and one LICENSE.txt.

    cargo build --release -p dogmind-app -p dogmind-setup
    python scripts/licenses.py
    python -I scripts/fetch_tools.py
    python -I scripts/package.py [path to straydog.exe] [path to the CUDA toolkit it was built with]
    python -I scripts/package.py --tools-only FOLDER     tools\\ alone, in FOLDER, for a test
    python -I scripts/package.py --setup-only FOLDER OUT.exe   an installer of FOLDER alone, for a test, from the release build of dogmind-setup or %DOGMIND_SETUP_STUB%

What comes out: the folder dist/DogmindApp, its zip, and the installer DogmindSetup-VERSION.exe, one file with that folder on its end.

tools\\ beside the exe holds what commands run with, so that a person installs nothing: Python with its modules, Node.js, Git. It comes from the pins in tools.lock.json; it is
an error for it to be missing, as an app without it behaves differently from what was tested.

LICENSE.txt holds this app's license, those of Dogmind, Straydog, Lucide and the CUDA toolkit whose libraries go beside the engine, and the list of Rust crates in the executable.
"""
import filecmp
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import time
import tomllib
import zipfile
from pathlib import Path, PureWindowsPath

ROOT = Path(__file__).resolve().parent.parent
PARTS = [
    ("Dogmind (this program)", ROOT / "LICENSE", None),
    ("Dogmind, the agent", ROOT / "assets" / "licenses" / "dogmind-LICENSE.txt", "This app's agent follows the design and behaviour of Dogmind's. Its checkpoint prompt and Codex policy are Dogmind's, and its system prompt and tool descriptions are shortened and rewritten from Dogmind's."),
    ("Straydog", ROOT / "assets" / "licenses" / "straydog-LICENSE.txt", "The engine, a separate program beside this one."),
    ("Lucide", ROOT / "assets" / "lucide" / "LICENSE", "The icons."),
    ("Rust crates in these executables", ROOT / "assets" / "licenses" / "third-party.txt", None),
    ("The WebView2 SDK", ROOT / "assets" / "licenses" / "webview2-sdk-LICENSE.txt", "Its loader, WebView2LoaderStatic.lib, is linked into the app, for the preview."),
    ("webview2-com and webview2-com-sys", ROOT / "assets" / "licenses" / "webview2-rs-LICENSE.txt", "These crates ship no license file: this is the one in their repository, github.com/wravery/webview2-rs."),
]


def fetched():
    """fetch_tools.py beside this file: `-I` keeps a script's folder off the path."""
    spec = importlib.util.spec_from_file_location("fetch_tools", ROOT / "scripts" / "fetch_tools.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# What a person's habits ask for and the embeddable build has no launcher for.
PY_SHIM = """import subprocess, sys
# `py`, `py -3`, `py -3.14`: there is one Python here.
arguments = sys.argv[1:]
if arguments and arguments[0][:2] == "-3":
    arguments = arguments[1:]
sys.exit(subprocess.call([sys.executable, *arguments]))
"""
# Beside the standard library's defaults: what `pip install` put in the workspace, wherever a command runs in it.
SITECUSTOMIZE = """import os, sys
_deps = os.environ.get("DOGMIND_PYDEPS")
if _deps:
    if _deps not in sys.path:
        sys.path.append(_deps)
    # On Windows a mode of 0o700 now means a list with only the user, SYSTEM and administrators on it. Under an AppContainer that shuts the process out of what it just made, so
    # tempfile.mkdtemp, and pip, and pytest's tmp_path, all fail with Permission denied. The sandbox's temp folder is its own already.
    _mkdir = os.mkdir
    def _open_mkdir(path, mode=0o777, *, dir_fd=None):
        return _mkdir(path, 0o777 if mode == 0o700 else mode, dir_fd=dir_fd)
    os.mkdir = _open_mkdir
del _deps
"""
# Started before every script under the sandbox, by NODE_OPTIONS.
NODE_FIX = r"""// Node looks at every folder on the way to a path, and an AppContainer may not look at the root of a drive, C:\Users, or the folders above its own temp folder. Whatever asks then
// fails with EPERM, from node's own realpath to npm's walk up the tree. They only want to know a folder is there, and it is: they get an answer about one this process can read.
"use strict";
const binding = process.binding("fs");
const windows = process.env.SystemRoot || "C:\\Windows";
const plain = (path) => path.replace(/^\\\\\?\\/, "").replace(/\//g, "\\").replace(/\\+$/, "").toLowerCase();
const within = [process.env.TEMP, process.env.USERPROFILE, process.cwd()].filter(Boolean).map((path) => plain(path) + "\\");
const onTheWay = (path) => {
  const wanted = plain(path);
  return /^[a-z]:$/.test(wanted) || within.some((place) => place.startsWith(wanted + "\\") && place !== wanted + "\\");
};
for (const name of ["lstat", "stat"]) {
  const original = binding[name];
  binding[name] = function (path, ...rest) {
    return original.call(this, typeof path === "string" && onTheWay(path) ? windows : path, ...rest);
  };
}
"""
GIT_KEEPS = re.compile(r"(?i)^(git(?!-credential-(manager|helper-selector))|lib(?!skia|harfbuzz)|zlib|brotli|psl|proxy-lookup|headless-git|printf_|scalar)")
SHIMS = {
    "pip.cmd": '@"%~dp0python.exe" -m pip %*\r\n',
    "pip3.cmd": '@"%~dp0python.exe" -m pip %*\r\n',
    "pytest.cmd": '@"%~dp0python.exe" -m pytest %*\r\n',
    "python3.cmd": '@"%~dp0python.exe" %*\r\n',
    "py.cmd": '@"%~dp0python.exe" "%~dp0py_shim.py" %*\r\n',
}
# Only for building against or testing the package itself, and a good part of the size.
NOT_SHIPPED_DIRS = {"tests", "testing", "__pycache__"}
NOT_SHIPPED_PATHS = ("numpy/_core/include", "numpy/_core/lib", "numpy/random/lib", "numpy/doc", "matplotlib/mpl-data/sample_data", "pandas/tests")
NOT_SHIPPED_SUFFIXES = (".pyi", ".lib", ".a", ".h", ".pxd", ".pyx")


def place_in(folder, name, archive):
    """Where a name in an archive lands, or the end of this if that is not inside the folder. As Windows reads it: a backslash is a separator here, wherever the archive was made, a drive or a
    root replaces the folder when joined to it, and `.. ` opens as `..`."""
    path = PureWindowsPath(name)
    if path.anchor or ":" in name or any(part != part.rstrip(". ") for part in path.parts):
        sys.exit(f"{archive}: {name} would land outside its folder")
    return folder / path.as_posix()


def extract(archive, target, strip=None, keep=lambda name: True):
    """By unpacking only: nothing in it is run."""
    with zipfile.ZipFile(archive) as zip_file:
        for item in zip_file.infolist():
            name = item.filename
            if strip:
                if not name.startswith(strip):
                    continue
                name = name[len(strip):]
            if not name or name.endswith("/") or not keep(name):
                continue
            place = place_in(target, name, archive)
            place.parent.mkdir(parents=True, exist_ok=True)
            with zip_file.open(item) as source, open(place, "wb") as out:
                shutil.copyfileobj(source, out)


def size_of(folder):
    return sum(file.stat().st_size for file in folder.rglob("*") if file.is_file())


def put(source, target):
    """`source` at `target`, even with the app running from this folder: a file that is the same already is left alone, and one in use (the
    app, its engine, their libraries, a font it loaded) is moved aside first as NAME.previous-TIME.EXT, which Windows allows for a file in use,
    where the copy running goes on reading it. The app sees its new build and restarts into it; what was moved aside goes at the next run."""
    source, target = Path(source), Path(target)
    if target.is_file() and target.stat().st_size == source.stat().st_size and filecmp.cmp(source, target, shallow=False):
        return
    try:
        shutil.copy(source, target)
    except PermissionError:
        target.rename(target.with_name(f"{target.stem}.previous-{time.strftime('%H%M%S')}{target.suffix}"))
        shutil.copy(source, target)


def remove(target):
    """A file that is no longer shipped: moved aside if it is in use, as `put` does."""
    try:
        target.unlink()
    except PermissionError:
        target.rename(target.with_name(f"{target.stem}.previous-{time.strftime('%H%M%S')}{target.suffix}"))


def install_wheel(wheel, site, files):
    """A wheel is a zip laid out for site-packages, with its `.data` to sort out. Returns the names of its licence files."""
    licenses = []
    with zipfile.ZipFile(wheel) as zip_file:
        for item in zip_file.infolist():
            name = item.filename
            if name.endswith("/"):
                continue
            parts = name.split("/")
            if parts[0].endswith(".data"):
                if parts[1] not in ("purelib", "platlib"):
                    continue
                parts = parts[2:]
                name = "/".join(parts)
            place = place_in(site, name, wheel)
            info = parts[0].endswith(".dist-info")
            if info and "licen" in name.lower() or (info and "copying" in name.lower()) or (info and "notice" in name.lower()):
                licenses.append(name)
            elif not info and (set(parts[:-1]) & NOT_SHIPPED_DIRS or name.startswith(NOT_SHIPPED_PATHS) or name.endswith(NOT_SHIPPED_SUFFIXES)):
                continue
            place.parent.mkdir(parents=True, exist_ok=True)
            with zip_file.open(item) as source, open(place, "wb") as out:
                shutil.copyfileobj(source, out)
            files.append(place)
    return licenses


def refuse_unless_tools(folder):
    """The folder is replaced whole, and `--tools-only` is told which on the command line: only an empty one, or an earlier tools folder, is this script's to take."""
    names = {item.name for item in folder.iterdir()}
    if names and not names & {"python", "node", "git"} or names - {"python", "node", "git", ".open-to-sandbox"}:
        sys.exit(f"{folder} holds other things than tools: name a folder of its own")


def assemble_tools(target):
    """tools\\python, tools\\node and tools\\git from the cache, verified by hash. Returns the licence parts to add, and prints what each thing costs."""
    if target.exists():
        refuse_unless_tools(target)
    fetch = fetched()
    lock = json.loads((ROOT / "scripts" / "tools.lock.json").read_text(encoding="utf-8"))
    cache = fetch.cache()

    def cached(item):
        path = cache / item["file"]
        if not path.is_file():
            sys.exit(f"{path} is missing: run python -I scripts/fetch_tools.py")
        if fetch.sha256(path) != item["sha256"]:
            sys.exit(f"{path} is not what tools.lock.json pins: delete it and run python -I scripts/fetch_tools.py")
        return path

    building = target.with_name(target.name + ".new")
    if building.exists():
        shutil.rmtree(building)
    parts, sizes = [], []
    # Python. No `._pth`: that file turns on isolated mode, which keeps a script's own folder off the path and ignores every PYTHON variable.
    python = building / "python"
    extract(cached(lock["python"]), python, keep=lambda name: name not in ("python.cat", "python3t.dll") and not name.endswith("._pth"))
    version = "".join(lock["python"]["version"].split(".")[:2])
    (python / f"python{version}.zip").stat()
    site = python / "Lib" / "site-packages"
    site.mkdir(parents=True)
    sizes.append((f"Python {lock['python']['version']}", size_of(python)))
    (site / "sitecustomize.py").write_text(SITECUSTOMIZE, encoding="utf-8")
    (python / "py_shim.py").write_text(PY_SHIM, encoding="utf-8")
    for name, text in SHIMS.items():
        (python / name).write_bytes(text.replace("\r\n", "\n").replace("\n", "\r\n").encode())
    parts.append((f"Python {lock['python']['version']}", python / "LICENSE.txt", "In tools\\python, from the embeddable build at python.org."))
    module_files = {}
    for wheel in lock["wheels"]:
        files = []
        for license_name in install_wheel(cached(wheel), site, files):
            parts.append((f"Python module {wheel['name']} {wheel['version']}: {Path(license_name).name}", site / license_name, None))
        module_files[wheel["name"]] = files
    # A chart's Japanese words come out as empty boxes in matplotlib's own font: BIZ UDGothic goes where it looks first.
    charts = site / "matplotlib" / "mpl-data"
    for item in lock["fonts"]["files"]:
        shutil.copy(cached(item), charts / "fonts" / "ttf" / item["file"])
    with (charts / "matplotlibrc").open("a", encoding="utf-8", newline="\n") as settings:
        settings.write("\nfont.sans-serif: BIZ UDGothic, DejaVu Sans\naxes.unicode_minus: False\n")
    # So that the first import of a large module is not its compiling, each time, in a folder a command cannot write in.
    subprocess.run([str(python / "python.exe"), "-I", "-m", "compileall", "-q", "-j", "2", "--invalidation-mode", "unchecked-hash", str(site)], check=True, cwd=python)
    for name, files in module_files.items():
        total = 0
        for file in files:
            total += file.stat().st_size
            if file.suffix == ".py":
                compiled = file.parent / "__pycache__" / f"{file.stem}.cpython-{version}.pyc"
                if compiled.is_file():
                    total += compiled.stat().st_size
        sizes.append((f"  {name} {next(wheel['version'] for wheel in lock['wheels'] if wheel['name'] == name)}", total))
    # Node.js: node.exe, npm and npx.
    node = building / "node"
    root = f"node-v{lock['node']['version']}-win-x64/"
    wanted = lambda name: name in ("node.exe", "npm", "npm.cmd", "npx", "npx.cmd", "LICENSE") or (name.startswith("node_modules/npm/") and not name.startswith(("node_modules/npm/docs/", "node_modules/npm/man/")))
    extract(cached(lock["node"]), node, strip=root, keep=wanted)
    (node / "sandbox_fix.cjs").write_text(NODE_FIX, encoding="utf-8", newline="\n")
    sizes.append((f"Node.js {lock['node']['version']} with npm and npx", size_of(node)))
    parts.append((f"Node.js {lock['node']['version']}", node / "LICENSE", "In tools\\node, from nodejs.org. npm and the packages under node_modules\\npm carry their licences beside them."))
    # Git.
    git = building / "git"
    # Without Git Credential Manager, more than half its size: it has nothing to do in a sandbox, and its way of asking is a window on the person's screen.
    extract(cached(lock["git"]), git, keep=lambda name: not (name.startswith("ucrt64/bin/") and not GIT_KEEPS.match(name[len("ucrt64/bin/"):])) and "credential-manager" not in name)
    config = git / "etc" / "gitconfig"
    # (and nothing of an installed Git's, whose system config this one would otherwise include)
    text = re.sub(r"(?ims)^\[credential\].*?(?=^\[|\Z)", "", config.read_text(encoding="utf-8"))
    config.write_text(re.sub(r"(?ims)^\[include\].*?(?=^\[|\Z)", "", text), encoding="utf-8", newline="\n")
    sizes.append((f"MinGit {lock['git']['version']}", size_of(git)))
    notice = building / "git-notice.txt"
    notice.write_text(
        f"MinGit {lock['git']['version']} is in tools\\git, from git-for-windows, without its Git Credential Manager and with that helper taken out of its configuration. It is under the GNU General Public License, version 2; its license is at tools\\git\\LICENSE.txt "
        f"where the archive has one, and the source of this version, and of what it is built with, is at https://github.com/git-for-windows/git/releases/tag/v{lock['git']['version']} and https://github.com/git-for-windows/git.\n",
        encoding="utf-8",
    )
    parts.append((f"Git for Windows (MinGit) {lock['git']['version']}", notice, None))
    for license_path in sorted(git.rglob("*")):
        if license_path.is_file() and license_path.name.upper().startswith(("LICENSE", "COPYING")) and license_path.stat().st_size < 200_000:
            parts.append((f"MinGit: {license_path.relative_to(git).as_posix()}", license_path, None))
    # What the licences say is kept before the old folder goes; the swap comes last, so that a failure above leaves what was there.
    texts = [(title, path.read_text(encoding="utf-8", errors="replace"), note) for title, path, note in parts]
    notice.unlink()
    if target.exists():
        refuse_unless_tools(target)
        shutil.rmtree(target)
    os.replace(building, target)
    total = size_of(target)
    for name, size in sizes:
        print(f"{size / 1048576:8.1f} MB  {name}")
    print(f"{total / 1048576:8.1f} MB  tools in all, {sum(1 for _ in target.rglob('*'))} files and folders")
    return texts


def toolkits():
    """The toolkits to look in: the one named on the command line, then what the environment names (CUDA_PATH may well name an older one than the engine's)."""
    named = [Path(sys.argv[2])] if len(sys.argv) > 2 else []
    return named + [Path(value) for key, value in sorted(os.environ.items()) if key.upper().startswith("CUDA_PATH")]


def cublas_for(straydog, places):
    """The cuBLAS files this engine imports and where they are, and the toolkit they came from: another version beside it would not load."""
    # (the engine names only cublas64_N.dll, which itself loads cublasLt64_N.dll)
    versions = sorted({version.decode() for version in re.findall(rb"cublas(?:Lt)?64_(\d+)\.dll", straydog.read_bytes(), re.IGNORECASE)})
    names = [name for version in versions for name in (f"cublas64_{version}.dll", f"cublasLt64_{version}.dll")]
    for toolkit in places:
        for folder in (toolkit / "bin" / "x64", toolkit / "bin"):
            if names and all((folder / name).is_file() for name in names):
                return [folder / name for name in names], toolkit
    return None, None


def make_setup(folder, out, version):
    """The installer: `dogmind-setup` itself, with the folder on its end, streamed and deflated by it. No tool beyond that, and what it left out of the folder (symbols, what is this machine's) it knows by itself. It then reads its work back."""
    stub = Path(os.environ.get("DOGMIND_SETUP_STUB") or ROOT / "target" / "release" / "dogmind-setup.exe")
    if not stub.is_file():
        sys.exit(f"{stub} is missing: run cargo build --release -p dogmind-setup")
    subprocess.run([str(stub), "--pack", str(folder), str(out), "--version", version], check=True)
    subprocess.run([str(stub), "--verify", str(out)], check=True)
    print(f"{out}: {out.stat().st_size / 1048576:.0f} MB")


def main():
    if len(sys.argv) > 2 and sys.argv[1] == "--tools-only":
        assemble_tools(Path(sys.argv[2]))
        return
    if len(sys.argv) > 3 and sys.argv[1] == "--setup-only":
        version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
        make_setup(Path(sys.argv[2]), Path(sys.argv[3]), version)
        return
    target = ROOT / "dist" / "DogmindApp"
    target.mkdir(parents=True, exist_ok=True)
    parts = list(PARTS)
    # (what an update moved aside is not part of what is shipped; one still in use goes next time)
    for old in [*target.glob("*.previous-*"), *target.glob("fonts/*.previous-*")]:
        try:
            old.unlink()
        except PermissionError:
            pass
    tool_licenses = assemble_tools(target / "tools")
    # The Japanese face, loaded by the app for itself and installed nowhere. Beside the exe and not in it: Windows maps a file as it needs it, and nothing of it comes to memory before.
    tools = fetched()
    lock = json.loads(tools.LOCK.read_text(encoding="utf-8"))["fonts"]
    (target / "fonts").mkdir(exist_ok=True)
    for item in lock["files"]:
        put(tools.fetch(item["url"], item["file"], item["sha256"]), target / "fonts" / item["file"])
    font_license = tools.fetch(lock["license"]["url"], lock["license"]["file"], lock["license"]["sha256"])
    parts.append(("BIZ UDGothic", font_license, "The Japanese type of the interface, in fonts\\, from Morisawa's open release (github.com/googlefonts/morisawa-biz-ud-gothic)."))
    # What a person reads first: how to get past SmartScreen, update and remove it.
    put(ROOT / "assets" / "READ-ME-FIRST.txt", target / "READ-ME-FIRST.txt")
    # (copy, not copy2: a file's date here says when it was installed, or an update looks as if it changed nothing; and the running app sees it as new)
    put(ROOT / "target" / "release" / "dogmind-app.exe", target / "dogmind-app.exe")
    straydog = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT.parent / "Straydog" / "target" / "release" / "straydog.exe"
    if straydog.is_file():
        put(straydog, target / "straydog.exe")
        # The cuBLAS it links, from the toolkit it was built with: a machine with none, or with another, needs its own beside it.
        libraries, toolkit = cublas_for(straydog, toolkits())
        if not libraries:
            sys.exit(f"the cuBLAS {straydog} imports is in none of {[str(place) for place in toolkits()]}: name its toolkit after it")
        for old in target.glob("cublas*64_*.dll"):
            if old.name not in {library.name for library in libraries}:
                remove(old)
        for library in libraries:
            put(library, target / library.name)
        parts.append(("NVIDIA CUDA Toolkit", toolkit / "LICENSE", "The cuBLAS libraries beside the engine."))
    else:
        print(f"no straydog.exe at {straydog}: the app will ask for one", file=sys.stderr)
    text = []
    for title, path, note in parts:
        text.append("=" * 72 + f"\n{title}\n" + (f"{note}\n" if note else "") + "=" * 72 + "\n\n" + path.read_text(encoding="utf-8").strip() + "\n\n")
    for title, body, note in tool_licenses:
        text.append("=" * 72 + f"\n{title}\n" + (f"{note}\n" if note else "") + "=" * 72 + "\n\n" + body.strip() + "\n\n")
    (target / "LICENSE.txt").write_text("".join(text), encoding="utf-8", newline="\r\n")
    print(f"{target}: " + ", ".join(sorted(file.name for file in target.iterdir())))
    # What a person is given. The debug symbols are not, but each release's are kept: a crash's addresses cannot be read without them.
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    symbols = ROOT / "dist" / f"symbols-{version}"
    symbols.mkdir(exist_ok=True)
    for pdb in (ROOT / "target" / "release" / "dogmind_app.pdb", straydog.with_name("straydog.pdb")):
        if pdb.is_file():
            shutil.copy(pdb, symbols / pdb.name)
    archive = ROOT / "dist" / f"DogmindApp-{version}.zip"
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as zip_file:
        for file in sorted(target.rglob("*")):
            # (`.open-to-sandbox` belongs to a folder that has been opened on this machine)
            if file.is_file() and file.suffix.lower() != ".pdb" and file.name != ".open-to-sandbox" and ".previous-" not in file.name:
                zip_file.write(file, Path("DogmindApp") / file.relative_to(target))
    print(f"{archive}: {archive.stat().st_size / 1048576:.0f} MB, {size_of(target) / 1048576:.0f} MB unpacked")
    make_setup(target, ROOT / "dist" / f"DogmindSetup-{version}.exe", version)


if __name__ == "__main__":
    main()
