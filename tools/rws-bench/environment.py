#!/usr/bin/env python3
"""Record reproducibility metadata without changing machine or server state."""
import argparse
import datetime
import hashlib
import json
import os
import pathlib
import platform
import subprocess


def read_text(path):
    try:
        return pathlib.Path(path).read_text(encoding="utf-8")
    except OSError:
        return ""


def digest(path):
    if path is None:
        return None
    value = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def command(*args):
    try:
        return subprocess.check_output(args, stderr=subprocess.DEVNULL, text=True, timeout=5).strip()
    except (OSError, subprocess.SubprocessError):
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--role", choices=("server", "generator"), required=True)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--binary", type=pathlib.Path, help="Server or load-generator binary to hash")
    parser.add_argument("--config", type=pathlib.Path, help="Configuration to hash; content is not recorded")
    parser.add_argument("--notes", default="", help="Non-secret workload, OS-cache and hardware notes")
    args = parser.parse_args()
    cpu_lines = read_text("/proc/cpuinfo").splitlines()
    cpu_model = next((line.split(":", 1)[1].strip() for line in cpu_lines
                      if line.lower().startswith("model name") and ":" in line), None)
    memory_lines = read_text("/proc/meminfo").splitlines()
    ram_kib = next((int(line.split()[1]) for line in memory_lines if line.startswith("MemTotal:")), None)
    release = {}
    for line in read_text("/etc/os-release").splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            if key in ("PRETTY_NAME", "VERSION_ID"):
                release[key] = value.strip('"')
    repo = str(pathlib.Path(__file__).resolve().parents[2])
    status = command("git", "-C", repo, "status", "--porcelain", "--untracked-files=normal")
    data = {
        "schema_version": 1,
        "recorded_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "role": args.role,
        "os": release,
        "kernel": platform.release(),
        "architecture": platform.machine(),
        "cpu_model": cpu_model,
        "logical_cpus": os.cpu_count(),
        "total_ram_kib": ram_kib,
        "rustc_version": command("rustc", "--version"),
        "git_revision": command("git", "-C", repo, "rev-parse", "HEAD"),
        "git_worktree_dirty": None if status is None else bool(status),
        "binary_sha256": digest(args.binary),
        "config_sha256": digest(args.config),
        "notes": args.notes,
    }
    with args.output.open("x", encoding="utf-8") as output:
        json.dump(data, output, indent=2, ensure_ascii=False)
        output.write("\n")


if __name__ == "__main__":
    main()
