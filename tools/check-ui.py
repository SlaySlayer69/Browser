#!/usr/bin/env python3
"""Static consistency checks between the HTML/JS chrome and the Rust host.

Run from the repository root:

    python3 tools/check-ui.py

The UI and the host are joined by two things the compiler cannot see: element
ids referenced from JavaScript, and the string names of the IPC commands and
events. Both fail silently at runtime — a typo'd id yields `null`, an unknown
command is dropped by `Command::parse`, and an event nobody listens for simply
never renders. This script turns all three into build-time errors.

It also runs `node --check` on every script when node is available.
"""

from __future__ import annotations

import glob
import os
import re
import subprocess
import shutil
import sys

# (page markup, scripts loaded by that page)
PAGES = [
    (["ui/newtab.html"], ["ui/newtab.js"]),
    (["ui/chrome.html"], ["ui/chrome.js"]),
    (["ui/pages/history.html"], ["ui/pages/common.js", "ui/pages/history.js"]),
    (["ui/pages/downloads.html"], ["ui/pages/common.js", "ui/pages/downloads.js"]),
    (["ui/pages/bookmarks.html"], ["ui/pages/common.js", "ui/pages/bookmarks.js"]),
    (["ui/pages/vault.html"], ["ui/pages/common.js", "ui/pages/vault.js"]),
]

IPC_SOURCE = "src/ipc/mod.rs"

# Rust variant `OmniboxSubmit` serialises as `omniboxSubmit`.
def to_camel(variant: str) -> str:
    return variant[0].lower() + variant[1:]


def rust_variants(source: str, header: str) -> set[str]:
    body = source.split(header)[1].split("\n}\n")[0]
    return {to_camel(v) for v in re.findall(r"^\s{4}([A-Z]\w+)", body, re.M)}


def js_files() -> list[str]:
    return sorted(glob.glob("ui/**/*.js", recursive=True))


def check_element_ids() -> list[str]:
    problems = []
    for htmls, scripts in PAGES:
        declared: set[str] = set()
        for path in htmls:
            declared |= set(re.findall(r'id="([^"]+)"', open(path).read()))

        used: set[str] = set()
        for path in scripts:
            src = open(path).read()
            used |= set(re.findall(r'getElementById\("([^"]+)"\)', src))
            used |= set(re.findall(r'\bel\("([^"]+)"\)', src))
            used |= set(re.findall(r'CD\.el\("([^"]+)"\)', src))

        missing = sorted(used - declared)
        if missing:
            problems.append(f"{htmls[0]}: scripts reference undeclared ids {missing}")
        else:
            print(f"  ok  {htmls[0]:<30} {len(used)} ids resolved")
    return problems


def check_ipc() -> list[str]:
    problems = []
    ipc = open(IPC_SOURCE).read()
    commands = rust_variants(ipc, "pub enum Command {")
    events = rust_variants(ipc, "pub enum Event<'a> {")

    # Commands the UI sends. The injected bridge script lives in Rust, so it is
    # scanned too.
    sent: set[str] = set()
    for path in js_files() + ["src/browser/app.rs"]:
        sent |= set(re.findall(r"""cmd:\s*['"](\w+)['"]""", open(path).read()))

    unknown = sorted(sent - commands)
    if unknown:
        problems.append(f"UI sends commands the host cannot parse: {unknown}")
    else:
        print(f"  ok  {len(sent)} commands sent by the UI exist in ipc::Command")

    # Events the UI listens for.
    listened: set[str] = set()
    for path in js_files():
        src = open(path).read()
        listened |= set(re.findall(r'\.evt === "(\w+)"', src))
        listened |= set(re.findall(r'onEvent\("(\w+)"', src))
        listened |= set(re.findall(r'case "(\w+)":', src))

    unknown = sorted(listened - events)
    if unknown:
        problems.append(f"UI listens for events the host never sends: {unknown}")
    else:
        print(f"  ok  {len(listened)} events the UI listens for exist in ipc::Event")

    return problems


def check_js_syntax() -> list[str]:
    node = shutil.which("node")
    if not node:
        print("  --  node not found, skipping syntax check")
        return []

    problems = []
    for path in js_files():
        result = subprocess.run([node, "--check", path], capture_output=True, text=True)
        if result.returncode != 0:
            problems.append(f"{path}: {result.stderr.strip().splitlines()[0]}")
    if not problems:
        print(f"  ok  {len(js_files())} scripts parse")
    return problems


def main() -> int:
    if not os.path.isdir("ui") or not os.path.isfile(IPC_SOURCE):
        print("run this from the repository root", file=sys.stderr)
        return 2

    problems: list[str] = []
    print("element ids")
    problems += check_element_ids()
    print("ipc contract")
    problems += check_ipc()
    print("javascript syntax")
    problems += check_js_syntax()

    if problems:
        print("\nFAILED:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1

    print("\nall UI checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
