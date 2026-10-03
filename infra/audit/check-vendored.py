#!/usr/bin/env python3
"""Audit vendored crates against their pinned upstream registry locks."""

import re
import subprocess
import sys
import tomllib
from pathlib import Path


CRATES_IO_REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
CHECKSUM = re.compile(r"[0-9a-f]{64}\Z")


def get_repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def load_toml(path: Path):
    try:
        with path.open("rb") as file:
            return tomllib.load(file)
    except FileNotFoundError:
        print(f"ERROR: Missing {path}", file=sys.stderr)
    except OSError as error:
        print(f"ERROR: Cannot read {path}: {error}", file=sys.stderr)
    except tomllib.TOMLDecodeError as error:
        print(f"ERROR: Invalid TOML in {path}: {error}", file=sys.stderr)
    return None


def get_manifest_package_info(manifest_path: Path):
    manifest = load_toml(manifest_path)
    if manifest is None:
        return None
    package = manifest.get("package")
    if not isinstance(package, dict):
        print(f"ERROR: No [package] table in {manifest_path}", file=sys.stderr)
        return None
    name, version = package.get("name"), package.get("version")
    if not isinstance(name, str) or not isinstance(version, str):
        print(f"ERROR: {manifest_path} [package] needs string name and version", file=sys.stderr)
        return None
    return name, version


def validate_audit_lock(lock_path: Path, expected_name: str, expected_version: str) -> bool:
    lock = load_toml(lock_path)
    if lock is None:
        return False
    packages = lock.get("package")
    if lock.get("version") != 3 or not isinstance(packages, list) or len(packages) != 1:
        print(f"ERROR: {lock_path} must be a version 3 lock with exactly one package", file=sys.stderr)
        return False
    package = packages[0]
    if not isinstance(package, dict):
        print(f"ERROR: {lock_path} has an invalid package entry", file=sys.stderr)
        return False
    if package.get("name") != expected_name or package.get("version") != expected_version:
        print(f"ERROR: {lock_path} package does not match {expected_name} {expected_version}", file=sys.stderr)
        return False
    if package.get("source") != CRATES_IO_REGISTRY:
        print(f"ERROR: {lock_path} must use the crates.io registry", file=sys.stderr)
        return False
    checksum = package.get("checksum")
    if not isinstance(checksum, str) or not CHECKSUM.fullmatch(checksum):
        print(f"ERROR: {lock_path} must contain a 64-character hexadecimal checksum", file=sys.stderr)
        return False
    return True


def run_cargo_audit(lock_path: Path) -> bool:
    try:
        result = subprocess.run(
            ["cargo", "audit", "--deny", "warnings", "--file", str(lock_path)],
            capture_output=True,
            text=True,
            timeout=300,
        )
    except FileNotFoundError:
        print("ERROR: cargo is not installed or not on PATH", file=sys.stderr)
        return False
    except OSError as error:
        print(f"ERROR: Cannot run cargo audit: {error}", file=sys.stderr)
        return False
    except subprocess.TimeoutExpired:
        print(f"ERROR: cargo audit timed out for {lock_path}", file=sys.stderr)
        return False
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    return result.returncode == 0


def main() -> int:
    vendor_dir = get_repo_root() / "vendor"
    if not vendor_dir.is_dir():
        print(f"ERROR: Missing vendor directory at {vendor_dir}", file=sys.stderr)
        return 1

    crate_dirs = sorted(path for path in vendor_dir.iterdir() if path.is_dir())
    if not crate_dirs:
        print(f"ERROR: No vendored crates found in {vendor_dir}", file=sys.stderr)
        return 1

    locks = []
    for crate_dir in crate_dirs:
        manifest_path = crate_dir / "Cargo.toml"
        lock_path = crate_dir / "upstream-audit.lock"
        package = get_manifest_package_info(manifest_path)
        if package is None or not validate_audit_lock(lock_path, *package):
            return 1
        locks.append(lock_path)

    all_passed = True
    for lock in locks:
        if not run_cargo_audit(lock):
            all_passed = False
    return 0 if all_passed else 1


if __name__ == "__main__":
    sys.exit(main())
