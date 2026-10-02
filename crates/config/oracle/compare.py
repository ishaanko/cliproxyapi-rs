#!/usr/bin/env python3
"""Diff the Go and Rust config oracle records (see dump.go and tests/oracle_dump.rs).

usage: compare.py <go-out-dir> <rust-out-dir>
Exits non-zero when any record differs.
"""
import json
import os
import sys


def diff(go, rust, path=""):
    out = []
    if isinstance(go, dict) and isinstance(rust, dict):
        for key in sorted(set(go) | set(rust)):
            if key not in go:
                out.append(f"{path}/{key}: only in rust = {json.dumps(rust[key])[:120]}")
            elif key not in rust:
                out.append(f"{path}/{key}: only in go = {json.dumps(go[key])[:120]}")
            else:
                out += diff(go[key], rust[key], f"{path}/{key}")
    elif isinstance(go, list) and isinstance(rust, list):
        if len(go) != len(rust):
            out.append(f"{path}: len go={len(go)} rust={len(rust)}")
        else:
            for i, (a, b) in enumerate(zip(go, rust)):
                out += diff(a, b, f"{path}[{i}]")
    elif go is None and rust in ([], {}):
        pass  # Go marshals a nil slice/map as null; the Rust view writes [] / {}.
    elif go != rust:
        out.append(f"{path}: go={json.dumps(go)[:100]} rust={json.dumps(rust)[:100]}")
    return out


def main():
    go_dir, rust_dir = sys.argv[1:3]
    total = 0
    for name in sorted(os.listdir(go_dir)):
        with open(os.path.join(go_dir, name)) as f:
            go = json.load(f)
        with open(os.path.join(rust_dir, name)) as f:
            rust = json.load(f)
        lines = diff(go, rust)
        total += len(lines)
        print(("DIFF " if lines else "ok   ") + name)
        for line in lines[:15]:
            print("     ", line)
    print("total differences:", total)
    sys.exit(1 if total else 0)


if __name__ == "__main__":
    main()
