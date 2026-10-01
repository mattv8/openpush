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

    def test_installs_template_actions_and_is_idempotent(self):
        self.assertEqual(self.install(), "installed")
        installed = self.read_config()
        template = json.loads((self.root / "infra/dev/openchamber-project.json").read_text())
        self.assertEqual(installed, template)
        self.assertEqual(self.install(), "unchanged")

    def test_preserves_unrelated_keys_and_actions_while_upgrading_recognized_action(self):
        config = {
            "version": 1,
            "customSetting": {"keep": True},
            "projectActions": [
                {"id": "other.action", "name": "Other", "command": "echo other", "icon": None},
                {
                    "id": "openpush.dev-up",
                    "name": "Old start label",
                    "command": "bash infra/dev/dev.sh dev-up",
                    "icon": "old-icon",
                },
            ],
        }
        self.config_path().parent.mkdir()
        self.config_path().write_text(json.dumps(config))

        self.assertEqual(self.install(), "updated")
        merged = self.read_config()
        self.assertEqual(merged["customSetting"], {"keep": True})
        self.assertEqual(merged["projectActions"][0], config["projectActions"][0])
        action = next(item for item in merged["projectActions"] if item["id"] == "openpush.dev-up")
        self.assertEqual(action["name"], "Dev: Start services")

    def test_rejects_unrelated_colliding_id_without_changing_file(self):
        config = {
            "version": 1,
            "projectActions": [
                {"id": "openpush.dev-up", "name": "Local", "command": "echo local", "icon": "tools"}
            ],
        }
        self.config_path().parent.mkdir()
        original = json.dumps(config)
        self.config_path().write_text(original)

        with self.assertRaisesRegex(self.installer.InstallError, "Rename the local action"):
            self.install()
        self.assertEqual(self.config_path().read_text(), original)

    def test_accepts_native_open_url_fields_in_template(self):
        template_path = self.root / "infra/dev/openchamber-project.json"
        template = json.loads(template_path.read_text())
        template["projectActions"][0].update({"autoOpenUrl": True, "openUrl": "https://example.test", "desktopOpenSshForward": "8080"})
        template_path.write_text(json.dumps(template))

        self.assertEqual(self.install(), "installed")
        action = self.read_config()["projectActions"][0]
        self.assertEqual(action["openUrl"], "https://example.test")

    def test_check_rejects_malformed_template_without_writing_config(self):
        (self.root / "infra/dev/openchamber-project.json").write_text("{not json")

        result = subprocess.run(
            [sys.executable, str(INSTALLER_PATH), "--root", str(self.root), "--check"],
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("malformed JSON", result.stderr)
        self.assertFalse(self.config_path().exists())

    def test_install_message_uses_template_action_count(self):
        template_path = self.root / "infra/dev/openchamber-project.json"
        template = json.loads(template_path.read_text())
        template["projectActions"].pop()
        template_path.write_text(json.dumps(template))

        result = subprocess.run(
            [sys.executable, str(INSTALLER_PATH), "--root", str(self.root), "--install"],
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Installed 13 actions", result.stdout)

    def test_sms_actions_use_safe_synthetic_defaults_and_ids_remain_stable(self):
        expected_command = 'bash infra/dev/dev.sh android-sms +15555550123 "synthetic OpenPush test message"'
        template = json.loads(TEMPLATE_PATH.read_text())
        tasks = json.loads(TASKS_PATH.read_text())["tasks"]

        self.assertEqual(len(template["projectActions"]), 14)
        self.assertEqual(len({action["id"] for action in template["projectActions"]}), 14)
        self.assertEqual(
            next(action for action in template["projectActions"] if action["id"] == "openpush.android-sms")["command"],
            expected_command,
        )
        self.assertEqual(next(task for task in tasks if task["label"].startswith("Android: Send"))["command"], expected_command)
        self.assertTrue(all(task.get("problemMatcher") == [] for task in tasks))

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


if __name__ == "__main__":
    unittest.main()
