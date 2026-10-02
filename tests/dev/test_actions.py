import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
INSTALLER_PATH = REPOSITORY_ROOT / "infra/dev/install-actions.py"
TEMPLATE_PATH = REPOSITORY_ROOT / "infra/dev/openchamber-project.json"
TASKS_PATH = REPOSITORY_ROOT / ".vscode/tasks.json"

LEGACY_ACTIONS = [
    ("openpush.dev-setup", "bash infra/dev/dev.sh dev-setup"),
    ("openpush.dev-actions", "bash infra/dev/dev.sh dev-actions"),
    ("openpush.dev-up", "bash infra/dev/dev.sh dev-up"),
    ("openpush.dev-down", "bash infra/dev/dev.sh dev-down"),
    ("openpush.dev-build", "bash infra/dev/dev.sh dev-build"),
    ("openpush.dev-test", "bash infra/dev/dev.sh dev-test"),
    ("openpush.dev-demo", "bash infra/dev/dev.sh dev-demo"),
    ("openpush.android-build", "bash infra/dev/dev.sh android-build"),
    ("openpush.android-emulator", "bash infra/dev/dev.sh android-emulator"),
    ("openpush.android-deploy", "bash infra/dev/dev.sh android-deploy"),
    ("openpush.android-smoke", "bash infra/dev/dev.sh android-smoke"),
    ("openpush.android-sms", 'bash infra/dev/dev.sh android-sms +15555550123 "synthetic OpenPush test message"'),
    ("openpush.desktop-dev", "bash infra/dev/dev.sh desktop-dev"),
    ("openpush.desktop-bundle", "bash infra/dev/dev.sh desktop-bundle"),
    ("openpush.desktop-run", "bash infra/dev/dev.sh desktop-run"),
    ("openpush.desktop-open", "bash infra/dev/dev.sh desktop-open"),
]
EXPECTED_ACTIONS = [
    ("openpush.dev-start", "Dev: Start development", "bash infra/dev/dev.sh dev-start", "play"),
    ("openpush.desktop-run", "Desktop: Rebuild and open", "bash infra/dev/dev.sh desktop-run", "play-circle"),
    ("openpush.android-run", "Android: Rebuild and open", "bash infra/dev/dev.sh android-run", "device-mobile"),
    ("openpush.dev-down", "Dev: Stop backend", "bash infra/dev/dev.sh dev-down", "stop-circle"),
]


def load_installer():
    spec = importlib.util.spec_from_file_location("install_actions", INSTALLER_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class InstallActionsTests(unittest.TestCase):
    def setUp(self):
        self.installer = load_installer()
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        template_dir = self.root / "infra/dev"
        template_dir.mkdir(parents=True)
        (template_dir / "openchamber-project.json").write_text(TEMPLATE_PATH.read_text())

    def tearDown(self):
        self.temp_dir.cleanup()

    def install(self):
        return self.installer.install_actions(self.root)[0]

    def config_path(self):
        return self.root / ".openchamber/project.json"

    def read_config(self):
        return json.loads(self.config_path().read_text())

    def write_config(self, config):
        self.config_path().parent.mkdir()
        self.config_path().write_text(json.dumps(config))

    def test_fresh_install_equals_template_and_rerun_is_unchanged(self):
        self.assertEqual(self.install(), "installed")
        self.assertEqual(self.read_config(), json.loads((self.root / "infra/dev/openchamber-project.json").read_text()))
        original = self.config_path().read_text()

        self.assertEqual(self.install(), "unchanged")
        self.assertEqual(self.config_path().read_text(), original)

    def test_template_accepts_native_open_url_fields(self):
        template_path = self.root / "infra/dev/openchamber-project.json"
        template = json.loads(template_path.read_text())
        template["projectActions"][0].update({"autoOpenUrl": True, "openUrl": "https://example.test", "desktopOpenSshForward": "8080"})
        template_path.write_text(json.dumps(template))

        self.assertEqual(self.install(), "installed")
        action = self.read_config()["projectActions"][0]
        self.assertEqual(action["openUrl"], "https://example.test")

    def test_cli_install_message_uses_dynamic_template_action_count(self):
        template_path = self.root / "infra/dev/openchamber-project.json"
        template = json.loads(template_path.read_text())
        template["projectActions"].pop()
        template_path.write_text(json.dumps(template))

        result = subprocess.run([sys.executable, str(INSTALLER_PATH), "--root", str(self.root), "--install"], check=False, capture_output=True, text=True)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"Installed {len(template['projectActions'])} actions", result.stdout)

    def test_editor_catalog_and_tasks_are_exact_four_action_toolbar_contract(self):
        template = json.loads(TEMPLATE_PATH.read_text())
        tasks = json.loads(TASKS_PATH.read_text())["tasks"]
        expected_actions = [
            {"id": action_id, "name": name, "command": command, "icon": icon, "platforms": ["macos", "linux"]}
            for action_id, name, command, icon in EXPECTED_ACTIONS
        ]

        self.assertEqual(template, {"version": 1, "projectActions": expected_actions})
        self.assertEqual([(task["label"], task["command"]) for task in tasks], [(name, command) for _, name, command, _ in EXPECTED_ACTIONS])
        self.assertTrue(all(task["options"] == {"cwd": "${workspaceFolder}"} for task in tasks))
        self.assertTrue(all(task["presentation"] == {"panel": "dedicated", "reveal": "always"} for task in tasks))
        self.assertTrue(all(task["problemMatcher"] == [] for task in tasks))

    def test_migrates_actual_legacy_sixteen_actions_to_four_and_is_idempotent(self):
        self.write_config({"version": 1, "projectActions": [
            {"id": action_id, "name": "Old", "command": command, "icon": "old"}
            for action_id, command in LEGACY_ACTIONS
        ]})

        self.assertEqual(self.install(), "updated")
        self.assertEqual(self.read_config()["projectActions"], json.loads(TEMPLATE_PATH.read_text())["projectActions"])
        self.assertEqual(self.install(), "unchanged")

    def test_preserves_customized_retired_command_unknown_namespace_and_user_keys(self):
        customized = {"id": "openpush.android-sms", "name": "Mine", "command": "echo mine", "custom": True}
        unknown = {"id": "openpush.experimental", "name": "Experimental", "command": "echo experimental"}
        unrelated = {"id": "other.action", "name": "Other", "command": "echo other"}
        self.write_config({"version": 1, "customSetting": {"keep": True}, "projectActions": [customized, unknown, unrelated]})

        self.assertEqual(self.install(), "updated")
        merged = self.read_config()
        self.assertEqual(merged["customSetting"], {"keep": True})
        self.assertEqual(merged["projectActions"][:3], [customized, unknown, unrelated])

    def test_refreshes_active_metadata_for_existing_dev_down_and_desktop_run_pairs(self):
        self.write_config({"version": 1, "projectActions": [
            {"id": "openpush.dev-down", "name": "Dev: Stop services", "command": "bash infra/dev/dev.sh dev-down", "icon": "old"},
            {"id": "openpush.desktop-run", "name": "Desktop: Build and run latest app", "command": "bash infra/dev/dev.sh desktop-run", "icon": "old"},
        ]})

        self.assertEqual(self.install(), "updated")
        self.assertEqual(self.read_config()["projectActions"], json.loads(TEMPLATE_PATH.read_text())["projectActions"])

    def test_rejects_active_command_collision_without_changing_file(self):
        config = {"version": 1, "projectActions": [
            {"id": "openpush.dev-down", "name": "Local", "command": "echo local", "icon": "tools"}
        ]}
        self.write_config(config)
        original = self.config_path().read_text()

        with self.assertRaisesRegex(self.installer.InstallError, "Rename the local action"):
            self.install()
        self.assertEqual(self.config_path().read_text(), original)

    def test_malformed_unknown_actions_are_preserved_not_silently_pruned(self):
        malformed = [{"name": "Missing id"}, {"id": ["not", "hashable"], "command": "echo list"}, {"id": "openpush.unknown"}]
        self.write_config({"version": 1, "projectActions": malformed})

        self.assertEqual(self.install(), "updated")
        self.assertEqual(self.read_config()["projectActions"][:3], malformed)

    def test_legacy_sms_pair_is_recognized_only_at_its_exact_historical_command(self):
        self.write_config({"version": 1, "projectActions": [
            {"id": "openpush.android-sms", "name": "Old", "command": LEGACY_ACTIONS[11][1]},
            {"id": "openpush.android-sms", "name": "Custom", "command": "bash infra/dev/dev.sh android-sms +1555 custom"},
        ]})

        self.assertEqual(self.install(), "updated")
        actions = self.read_config()["projectActions"]
        self.assertEqual(actions[0]["id"], "openpush.android-sms")
        self.assertEqual(actions[0]["command"], "bash infra/dev/dev.sh android-sms +1555 custom")
        self.assertEqual(len(actions), 5)

    def test_rejects_malformed_config_without_changing_file(self):
        self.config_path().parent.mkdir()
        original = "{not json"
        self.config_path().write_text(original)

        with self.assertRaisesRegex(self.installer.InstallError, "malformed JSON"):
            self.install()
        self.assertEqual(self.config_path().read_text(), original)

    def test_rejects_symlinked_shared_config(self):
        target = self.root / "local-project.json"
        target.write_text('{"version": 1, "projectActions": []}')
        self.config_path().parent.mkdir()
        self.config_path().symlink_to(target)

        with self.assertRaisesRegex(self.installer.InstallError, "symlink"):
            self.install()
        self.assertTrue(self.config_path().is_symlink())

    def test_check_rejects_malformed_template_without_writing_config(self):
        (self.root / "infra/dev/openchamber-project.json").write_text("{not json")
        result = subprocess.run([sys.executable, str(INSTALLER_PATH), "--root", str(self.root), "--check"], check=False, capture_output=True, text=True)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("malformed JSON", result.stderr)
        self.assertFalse(self.config_path().exists())


if __name__ == "__main__":
    unittest.main()
