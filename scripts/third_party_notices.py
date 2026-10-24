"""Writes a Markdown list of every Rust crate compiled into the executable with its declared license."""
import json
import subprocess
import sys
from pathlib import Path


def main(root, output):
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--filter-platform", "x86_64-pc-windows-msvc"],
        cwd=root,
    ))
    resolve = meta["resolve"]
    packages = {p["id"]: p for p in meta["packages"]}
    root_id = resolve["root"]
    nodes = {n["id"]: n for n in resolve["nodes"]}
    seen, stack = set(), [root_id]
    while stack:
        node = nodes[stack.pop()]
        for dep in node["deps"]:
            if any(k["kind"] in (None, "normal") for k in dep["dep_kinds"]) and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                stack.append(dep["pkg"])
    rows = sorted((packages[i]["name"], packages[i]["version"], packages[i].get("license") or "see crate", packages[i].get("repository") or "") for i in seen)
    lines = [
        "# Third-party Rust crates",
        "",
        "Crates statically linked into PS5LanCapture.exe (generated from Cargo.lock).",
        "",
        "| Crate | Version | License | Repository |",
        "|---|---|---|---|",
    ]
    lines += [f"| {n} | {v} | {l} | {r} |" for n, v, l, r in rows]
    Path(output).write_text("\n".join(lines) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
