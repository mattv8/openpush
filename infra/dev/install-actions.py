#!/usr/bin/env python3
"""Safely install this checkout's OpenChamber shared project actions."""

from __future__ import annotations

import argparse
import json
import os
import stat
import sys
import tempfile
from pathlib import Path
from typing import Any


class InstallError(Exception):
    """A configuration that must not be changed automatically."""


def reject_symlink(path: Path, label: str) -> None:
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError:
        return
    if stat.S_ISLNK(mode):
        raise InstallError(f"refusing symlinked {label}: {path}")


def load_json(path: Path, label: str) -> dict[str, Any]:
    reject_symlink(path, label)
    try:
        if not stat.S_ISREG(path.lstat().st_mode):
            raise InstallError(f"refusing non-regular {label}: {path}")
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise
    except (OSError, json.JSONDecodeError) as error:
        raise InstallError(f"{label} contains malformed JSON: {path}: {error}") from error
    if not isinstance(value, dict):
        raise InstallError(f"{label} must be a JSON object: {path}")
    return value


def validate_action(action: Any, source: str) -> dict[str, Any]:
    if not isinstance(action, dict):
        raise InstallError(f"{source} action must be an object")
    allowed = {
        "id", "name", "command", "icon", "platforms", "runIn",
        "autoOpenUrl", "openUrl", "desktopOpenSshForward",
    }
    unknown = set(action) - allowed
    if unknown:
        raise InstallError(f"{source} action has unsupported fields: {', '.join(sorted(unknown))}")
    for key in ("id", "name", "command"):
        if not isinstance(action.get(key), str) or not action[key].strip():
            raise InstallError(f"{source} action requires a non-empty {key}")
    if action.get("icon") is not None and not isinstance(action.get("icon"), str):
        raise InstallError(f"{source} action icon must be a string or null")
    if "platforms" in action:
        platforms = action["platforms"]
        if not isinstance(platforms, list) or any(item not in {"macos", "linux", "windows"} for item in platforms):
            raise InstallError(f"{source} action platforms must contain only supported platforms")
    if "runIn" in action and action["runIn"] != "parent":
        raise InstallError(f"{source} action runIn must be parent when present")
    if "autoOpenUrl" in action and action["autoOpenUrl"] is not True:
        raise InstallError(f"{source} action autoOpenUrl must be true when present")
    for key in ("openUrl", "desktopOpenSshForward"):
        if key in action and not isinstance(action[key], str):
            raise InstallError(f"{source} action {key} must be a string when present")
    return action


def load_template(root: Path) -> dict[str, Any]:
    template_path = root / "infra/dev/openchamber-project.json"
    template = load_json(template_path, "OpenChamber action template")
    if template.get("version") != 1:
        raise InstallError("OpenChamber action template must have version 1")
    actions = template.get("projectActions")
    if not isinstance(actions, list) or not actions:
        raise InstallError("OpenChamber action template must contain projectActions")
    ids: set[str] = set()
    for action in actions:
        validate_action(action, "OpenChamber action template")
        if action["id"] in ids:
            raise InstallError(f"OpenChamber action template duplicates id: {action['id']}")
        ids.add(action["id"])
    return template


def is_recognized_owned_action(existing: dict[str, Any], template_action: dict[str, Any]) -> bool:
    """Recognize actions by their immutable host command, not namespace alone."""
    return (
        existing.get("id") == template_action["id"]
        and existing.get("command") == template_action["command"]
    )


def atomic_write(path: Path, value: dict[str, Any]) -> None:
    directory = path.parent
    reject_symlink(directory, "OpenChamber configuration directory")
    if not directory.exists():
        directory.mkdir(mode=0o700)
    elif not directory.is_dir():
        raise InstallError(f"OpenChamber configuration directory is not a directory: {directory}")
    reject_symlink(path, "OpenChamber shared configuration")
    encoded = json.dumps(value, indent=2, sort_keys=False) + "\n"
    descriptor, temporary_name = tempfile.mkstemp(prefix=".project.", suffix=".tmp", dir=directory)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            output.write(encoded)
            output.flush()
            os.fsync(output.fileno())
        os.chmod(temporary, 0o600)
        reject_symlink(path, "OpenChamber shared configuration")
        os.replace(temporary, path)
    except OSError as error:
        raise InstallError(f"could not atomically write OpenChamber shared configuration: {error}") from error
    finally:
        temporary.unlink(missing_ok=True)


def install_actions(root: Path) -> tuple[str, int]:
    root = root.resolve()
    template = load_template(root)
    config_path = root / ".openchamber/project.json"
    try:
        existing = load_json(config_path, "OpenChamber shared configuration")
    except FileNotFoundError:
        atomic_write(config_path, template)
        return "installed", len(template["projectActions"])

    existing_actions = existing.get("projectActions", [])
    if not isinstance(existing_actions, list):
        raise InstallError("OpenChamber shared configuration projectActions must be an array")
    template_by_id = {action["id"]: action for action in template["projectActions"]}
    merged_actions: list[Any] = []
    replaced: set[str] = set()
    for action in existing_actions:
        if not isinstance(action, dict):
            merged_actions.append(action)
            continue
        action_id = action.get("id")
        template_action = template_by_id.get(action_id)
        if template_action is None:
            merged_actions.append(action)
        elif is_recognized_owned_action(action, template_action):
            merged_actions.append(template_action)
            replaced.add(action_id)
        else:
            raise InstallError(
                f"OpenChamber action '{action_id}' has a customized command and will not be overwritten. "
                "Rename the local action to a different ID to keep it, or restore the template command."
            )
    for action in template["projectActions"]:
        if action["id"] not in replaced:
            merged_actions.append(action)

    merged = dict(existing)
    merged.setdefault("version", 1)
    if merged["version"] != 1:
        raise InstallError("OpenChamber shared configuration version must be 1")
    merged["projectActions"] = merged_actions
    if merged == existing:
        return "unchanged", len(template["projectActions"])
    atomic_write(config_path, merged)
    return "updated", len(template["projectActions"])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2], help=argparse.SUPPRESS)
    command = parser.add_mutually_exclusive_group()
    command.add_argument("--install", action="store_true", help="install or update shared project actions")
    command.add_argument("--check", action="store_true", help="validate the tracked action template")
    args = parser.parse_args()
    try:
        if args.check:
            load_template(args.root.resolve())
            print("OpenChamber action template is valid")
        else:
            result, action_count = install_actions(args.root)
            messages = {
                "installed": f"Installed {action_count} actions to .openchamber/project.json",
                "updated": "Updated OpenChamber actions in .openchamber/project.json",
                "unchanged": "OpenChamber actions already up to date",
            }
            print(messages[result])
    except InstallError as error:
        print(f"install-actions: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
