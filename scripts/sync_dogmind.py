#!/usr/bin/env python3
"""Generate assets/dogmind from a Dogmind checkout: the system prompt and the tool schemas the console implements.

Run with Dogmind's own Python, which can import it:

    ..\\Dogmind\\.venv\\Scripts\\python.exe scripts\\sync_dogmind.py [--check] [--dogmind ..\\Dogmind]

--check rewrites nothing and fails when the assets differ from what Dogmind now generates. Text and schemas come from
Dogmind; what the console leaves out or changes is listed here and nowhere else, so that a new Dogmind sentence or
parameter arrives by running this script. A tool or parameter named below that Dogmind no longer has is an error.
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets" / "dogmind"

# The tools the console implements, with the parameters it honours.
TOOLS = {
    "read_file": ["path", "start_line", "limit"],
    "list_files": ["path", "recursive"],
    "search_files": ["pattern", "literal", "path", "file_glob", "timeout_seconds"],
    "apply_patch": ["edits", "expected_files", "purpose"],
    "create_file": ["path", "content", "purpose", "if_exists"],
    "execute_command": ["command", "cwd", "timeout_seconds", "purpose", "risk_level"],
    "show_diff": None,  # every parameter
    "todo": None,
}

# Sentences about tools the console does not have.
DROPPED = re.compile(
    r"inspect_command|workspace_depgraph|check_syntax|memory_search|memory_remember|session_search|skill_view"
    r"|skill_search|Skill |web_search|web_read|mem://|browser tool|lint_diagnostics|read lease",
)
# Reworded, as the sentence stays true of the console only without these words.
REPLACED = [
    (re.compile(r"read_file, search_files,\s+list_files, workspace_depgraph,.*?skill_view\)", re.S), "read_file, search_files, list_files)"),
    (" and citation tokens (`dh://`, `hp://`, `mem://`, `sess://`, `kp://`, `skill://`)", ""),
    ('memory, the "Dogs know you" capsule, or earlier turns', "memory or earlier turns"),
    (" For read-only git/test queries, use `inspect_command` instead — it does not consume the user's attention budget.", ""),
    ("; use read-only shell inspection only for queries its contract supports", ""),
    # Dogmind has inspect_command for read-only queries; here, commands are the only way to ask them.
    ("- Use `execute_command` ONLY for state-changing operations: running tests, installing packages, building, git commit/push, package-manager mutations.",
     "- Use `execute_command` to run programs and scripts, tests, builds, package managers and git, and for any check the other tools do not cover (`git log`, `node --check file.js`, `python -c ...`). Verify what you wrote by running it when it can be run."),
    (re.compile(r"Use the available tool that owns the question: file contents, code structure, memory, knowledge, or conversation history\. Prefer redaction-aware data tools; raw SQLite is reserved for cases where no redaction-aware tool can answer and access is authorized\. Explain the observed limitation; fabricating an absent result is forbidden\. Retrieve past conversations only when the question needs conversation history\."),
     "Use the available tool that owns the question. Explain the observed limitation; fabricating an absent result is forbidden."),
    ("comes only from the user's persona setting", "comes only from the user's own instructions"),
]
# Tool descriptions, likewise. A pattern that no longer matches is an error: Dogmind reworded it, so look again.
DESCRIPTIONS = {
    "apply_patch": [(r" Use the legacy ``patch`` string only.*$", "")],
    "read_file": [(r"Read paginated text, a symbol definition, an image, PDF pages, or a spreadsheet preview\.", "Read paginated text.")],
    "search_files": [(r" ``queries`` fans out multiple patterns in one call\.", ""), (r" For callers, callees, imports, or impact, use workspace_depgraph — not grep\.", "")],
    "execute_command": [
        (r"Use read-only inspection tools for status and queries\. ", ""),
        (r" Prefer managed web tools for HTTP reads\.", ""),
        (r"For directory listings use list_files or a single inspect_command `dir` call \(/b /a:-d includes file counts\); list_files summary=true gives file counts, sizes and extensions of a tree in one call; `python` / `python3` both run Dogmind's own interpreter when none is on PATH; do not use", "For directory listings use list_files; do not use"),
        (r" Do not retry the same listing with different cwd or pipes; use the inspection tools, one command per call without pipes\.", ""),
        (r"Do not redirect output to a log merely to inspect it; Dogmind captures stdout/stderr\.", "Do not redirect output to a log merely to inspect it; stdout and stderr are captured."),
    ],
}
SECTIONS = ["_SP_HEADER", "_SP_TOOL_USAGE", "_SP_PARALLEL_TOOL_CALLS", "_SP_TOOL_INVOCATION", "_SP_COMMAND_RISK", "_SP_COMMON_BEHAVIOR"]


def prune_text(text):
    for old, new in REPLACED:
        text = old.sub(new, text) if isinstance(old, re.Pattern) else text.replace(old, new)
    kept, dropping = [], False
    for line in text.splitlines():
        if line.startswith("- ") or line.startswith("## ") or not line.strip():
            dropping = bool(line.startswith("- ") and DROPPED.search(line))
        if not dropping:
            kept.append(line)
    return "\n".join(kept) + "\n"


def prune_tool(schema):
    function = schema["function"]
    text = json.dumps(function["description"], ensure_ascii=False)
    for pattern, replacement in DESCRIPTIONS.get(function["name"], []):
        target = "description" if "Do not redirect" not in pattern else None
        if target:
            function["description"], count = re.subn(pattern, replacement, function["description"], flags=re.S)
        else:
            parameter = function["parameters"]["properties"]["command"]
            parameter["description"], count = re.subn(pattern, replacement, parameter["description"], flags=re.S)
        if not count:
            sys.exit(f"Dogmind reworded {function['name']}, {pattern!r} no longer matches {text[:80]}: look at what changed")
    keep = TOOLS[function["name"]]
    if keep is None:
        return schema
    properties = function["parameters"].get("properties", {})
    missing = [name for name in keep if name not in properties]
    if missing:
        sys.exit(f"Dogmind's {function['name']} no longer has {missing}: look at what changed and update TOOLS")
    function["parameters"]["properties"] = {name: properties[name] for name in keep}
    if "required" in function["parameters"]:
        function["parameters"]["required"] = [name for name in function["parameters"]["required"] if name in keep]
    return schema


def refuse_sandbox_setup():
    """Generating text must change nothing on the machine: a sandbox ACL or profile this script would create is an error."""
    if sys.platform != "win32":
        return
    from dogmind.platform.primitives import _windows_sandbox_setup as setup

    def refused(*args, **kwargs):
        sys.exit(f"Dogmind began setting up its sandbox while generating assets {args[:1]}: find the new path to it")

    setup._grant_path_access = refused
    setup._create_app_container_profile = refused


def generate(dogmind):
    sys.path.insert(0, str(dogmind / "src"))
    refuse_sandbox_setup()
    from dogmind.core import agent_compaction, prompt
    from dogmind.llm import codex_app_server as codex
    from dogmind.tools.diff import get_show_diff_tool
    from dogmind.tools.filesystem import get_create_file_tool, get_list_files_tool, get_read_file_tool, get_search_files_tool
    from dogmind.tools.patch import get_apply_patch_tool
    from dogmind.tools.shell import get_execute_command_tool
    from dogmind.tools.todo_tools import get_todo_tool

    # One getter a tool, not create_default_registry: building inspect_command's schema probes Dogmind's Windows sandbox,
    # and that probe writes ACLs.
    getters = [get_apply_patch_tool, get_create_file_tool, get_execute_command_tool, get_list_files_tool, get_read_file_tool,
               get_search_files_tool, get_show_diff_tool, get_todo_tool]
    schemas = {schema["function"]["name"]: schema for schema in (getter().to_openai_schema() for getter in getters)}
    missing = [name for name in TOOLS if name not in schemas]
    if missing:
        sys.exit(f"Dogmind no longer has the tools {missing}: look at what changed and update TOOLS")
    tools = [prune_tool(schemas[name]) for name in sorted(TOOLS)]
    commit = subprocess.run(["git", "-C", str(dogmind), "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
    return {
        "system_prompt.md": prune_text("".join(getattr(prompt, name) for name in SECTIONS)),
        "checkpoint_prompt.md": agent_compaction._MAIN_MODEL_CHECKPOINT_PROMPT,
        "tools.json": json.dumps(tools, indent=1, ensure_ascii=False, sort_keys=True) + "\n",
        "codex.json": json.dumps({"disabled_features": list(codex._DISABLED_FEATURES), "forbidden_method_parts": list(codex._FORBIDDEN_METHOD_PARTS), "context_fallback": codex.CODEX_SUBSCRIPTION_CONTEXT_FALLBACK}, indent=1) + "\n",
        "SOURCE": commit + "\n",
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dogmind", type=Path, default=ROOT.parent / "Dogmind")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    files = generate(args.dogmind.resolve())
    changed = [name for name, text in files.items() if name != "SOURCE" and not ((ASSETS / name).is_file() and (ASSETS / name).read_text(encoding="utf-8") == text)]
    if args.check:
        if changed:
            sys.exit(f"Dogmind changed {changed}: run scripts/sync_dogmind.py and look at the diff")
        print("assets match Dogmind", files["SOURCE"].strip())
        return
    ASSETS.mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
        (ASSETS / name).write_text(text, encoding="utf-8", newline="\n")
    print("wrote", ", ".join(files), "from", files["SOURCE"].strip(), "; changed:", changed or "nothing")


if __name__ == "__main__":
    main()
