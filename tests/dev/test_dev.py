import os
import shutil
import subprocess
import tempfile
import time
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class DevScriptTests(unittest.TestCase):
    def run_script(self, *args, env=None):
        return subprocess.run(
            ["bash", "infra/dev/dev.sh", *args], cwd=ROOT, text=True,
            capture_output=True, env=env,
        )

    def test_unknown_recipe_fails_without_evaluating_argument(self):
        result = self.run_script("not-a-recipe; touch SHOULD_NOT_EXIST")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((ROOT / "SHOULD_NOT_EXIST").exists())

    def test_setup_missing_installer_is_actionable_and_preserves_existing_env(self):
        with tempfile.TemporaryDirectory() as directory:
            existing = Path(directory) / ".env"
            existing.write_text("KEEP_ME=1\n")
            env = os.environ | {"OPENPUSH_REPOSITORY_ROOT": directory}
            result = self.run_script("dev-setup", env=env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("install-actions.py", result.stderr)
            self.assertEqual(existing.read_text(), "KEEP_ME=1\n")

    def test_setup_creates_private_random_synthetic_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            installer = root / "infra/dev/install-actions.py"
            installer.parent.mkdir(parents=True)
            installer.write_text("print('actions installed')\n")
            result = self.run_script("dev-setup", env=os.environ | {"OPENPUSH_REPOSITORY_ROOT": directory})
            self.assertEqual(result.returncode, 0, result.stderr)
            configuration = (root / ".env")
            self.assertEqual(configuration.stat().st_mode & 0o777, 0o600)
            values = configuration.read_text()
            self.assertIn("POSTGRES_PASSWORD=synthetic-", values)
        self.assertNotIn("replace-with-", values)

    def test_actions_refresh_installs_actions_without_creating_or_modifying_env(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = ROOT / "infra/dev"
            destination = root / "infra/dev"
            destination.mkdir(parents=True)
            for name in ("install-actions.py", "openchamber-project.json"):
                shutil.copy(source / name, destination / name)

            missing = self.run_script("dev-actions", env=os.environ | {"OPENPUSH_REPOSITORY_ROOT": directory})
            self.assertEqual(missing.returncode, 0, missing.stderr)
            self.assertFalse((root / ".env").exists())
            self.assertTrue((root / ".openchamber/project.json").exists())

            existing = root / ".env"
            existing.write_text("KEEP_ME=1\n")
            refreshed = self.run_script("dev-actions", env=os.environ | {"OPENPUSH_REPOSITORY_ROOT": directory})
            self.assertEqual(refreshed.returncode, 0, refreshed.stderr)
            self.assertEqual(existing.read_text(), "KEEP_ME=1\n")

    def test_just_desktop_run_builds_before_open_and_stops_on_build_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "infra/dev").mkdir(parents=True)
            shutil.copy(ROOT / "justfile", root / "justfile")
            helper = root / "infra/dev/desktop.sh"
            order = root / "order"
            helper.write_text(
                "#!/usr/bin/env bash\n"
                "printf '%s\\n' \"$1\" >> \"$OPENPUSH_ORDER\"\n"
                "[ \"$1\" != build ] || [ \"${FAIL_BUILD:-}\" != 1 ]\n"
            )
            helper.chmod(0o755)
            env = os.environ | {"OPENPUSH_ORDER": str(order)}
            success = subprocess.run(["just", "desktop-run"], cwd=root, env=env, text=True, capture_output=True)
            self.assertEqual(success.returncode, 0, success.stderr)
            self.assertEqual(order.read_text().splitlines(), ["build", "open"])

            order.unlink()
            failure = subprocess.run(["just", "desktop-run"], cwd=root, env=env | {"FAIL_BUILD": "1"}, text=True, capture_output=True)
            self.assertNotEqual(failure.returncode, 0)
            self.assertEqual(order.read_text().splitlines(), ["build"])

    def test_container_runner_refuses_demo_outside_isolated_context(self):
        result = subprocess.run(
            ["bash", "infra/dev/container-run.sh", "demo"], cwd=ROOT,
            text=True, capture_output=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dev-demo", result.stderr)

    def test_source_exclusions_keep_example_but_exclude_local_env(self):
        exclusions = (ROOT / "infra/dev/source-excludes.txt").read_text()
        self.assertIn(".env*", exclusions)
        self.assertIn(".env.example", exclusions)
        self.assertIn("local.properties", exclusions)

    def test_tar_exclusions_keep_tracked_infra_build(self):
        with tempfile.TemporaryDirectory() as directory:
            root, output = Path(directory) / "source", Path(directory) / "output"
            (root / "infra/build").mkdir(parents=True)
            (root / "apps/android/app/build").mkdir(parents=True)
            (root / ".env").write_text("secret")
            (root / ".env.example").write_text("example")
            (root / "local.properties").write_text("sdk.dir=secret")
            (root / "infra/build/tool.sh").write_text("#!/bin/sh\n")
            (root / "infra/build/.env").write_text("nested-secret")
            (root / "apps/android/app/build/ignored.txt").write_text("ignored")
            output.mkdir()
            archive = subprocess.run(["tar", "-X", str(ROOT / "infra/dev/source-excludes.txt"), "-cf", "-", "."], cwd=root, check=True, stdout=subprocess.PIPE).stdout
            subprocess.run(["tar", "-xf", "-"], cwd=output, input=archive, check=True)
            self.assertFalse((output / ".env").exists())
            self.assertFalse((output / "local.properties").exists())
            self.assertTrue((output / "infra/build/tool.sh").exists())
            self.assertFalse((output / "infra/build/.env").exists())
            self.assertFalse((output / "apps/android/app/build/ignored.txt").exists())

    def test_missing_source_does_not_wipe_workspace(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workspace = root / "workspace"
            workspace.mkdir()
            (workspace / "keep.txt").write_text("keep")
            env = os.environ | {"OPENPUSH_SOURCE_ROOT": str(root / "missing"), "OPENPUSH_WORKSPACE": str(workspace), "CARGO_HOME": str(root / "cargo"), "PNPM_HOME": str(root / "pnpm"), "COREPACK_HOME": str(root / "corepack")}
            for key in ("CARGO_HOME", "PNPM_HOME", "COREPACK_HOME"):
                Path(env[key]).mkdir()
            result = subprocess.run(["bash", "infra/dev/container-run.sh", "build", "server"], cwd=ROOT, env=env, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue((workspace / "keep.txt").exists())

    @unittest.skipUnless(shutil.which("flock"), "flock is provided by the Linux tooling image")
    def test_container_runner_serializes_complete_runs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, workspace, tools = root / "source", root / "workspace", root / "tools"
            (source / "infra/dev").mkdir(parents=True)
            shutil.copy(ROOT / "infra/dev/source-excludes.txt", source / "infra/dev/source-excludes.txt")
            (source / "Cargo.toml").write_text("[workspace]\n")
            tools.mkdir()
            cargo = tools / "cargo"
            cargo.write_text("#!/bin/sh\nsleep .25\n")
            cargo.chmod(0o755)
            env = os.environ | {"OPENPUSH_SOURCE_ROOT": str(source), "OPENPUSH_WORKSPACE": str(workspace), "CARGO_HOME": str(root / "cargo"), "PNPM_HOME": str(root / "pnpm"), "COREPACK_HOME": str(root / "corepack"), "PATH": str(tools) + os.pathsep + os.environ["PATH"]}
            for key in ("CARGO_HOME", "PNPM_HOME", "COREPACK_HOME"):
                Path(env[key]).mkdir()
            started = time.monotonic()
            first = subprocess.Popen(["bash", "infra/dev/container-run.sh", "build", "server"], cwd=ROOT, env=env)
            second = subprocess.Popen(["bash", "infra/dev/container-run.sh", "build", "server"], cwd=ROOT, env=env)
            self.assertEqual(first.wait(timeout=5), 0)
            self.assertEqual(second.wait(timeout=5), 0)
            self.assertGreaterEqual(time.monotonic() - started, 0.45)

    @unittest.skipUnless(shutil.which("flock"), "flock is provided by the Linux tooling image")
    def test_workspace_lock_timeout_is_validated_and_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, workspace = root / "source", root / "workspace"
            (source / "infra/dev").mkdir(parents=True)
            shutil.copy(ROOT / "infra/dev/source-excludes.txt", source / "infra/dev/source-excludes.txt")
            for name in ("cargo", "pnpm", "corepack"):
                (root / name).mkdir()
            env = os.environ | {"OPENPUSH_SOURCE_ROOT": str(source), "OPENPUSH_WORKSPACE": str(workspace), "CARGO_HOME": str(root / "cargo"), "PNPM_HOME": str(root / "pnpm"), "COREPACK_HOME": str(root / "corepack"), "OPENPUSH_WORKSPACE_LOCK_TIMEOUT": "0"}
            invalid = subprocess.run(["bash", "infra/dev/container-run.sh", "build", "server"], cwd=ROOT, env=env, text=True, capture_output=True)
            self.assertNotEqual(invalid.returncode, 0)
            self.assertIn("LOCK_TIMEOUT", invalid.stderr)
            workspace.mkdir()
            (workspace / "target").mkdir()
            holder = subprocess.Popen(["flock", "-x", str(workspace / "target/.openpush-run.lock"), "sleep", "3"])
            try:
                time.sleep(0.05)
                env["OPENPUSH_WORKSPACE_LOCK_TIMEOUT"] = "1"
                timed = subprocess.run(["bash", "infra/dev/container-run.sh", "build", "server"], cwd=ROOT, env=env, text=True, capture_output=True)
                self.assertNotEqual(timed.returncode, 0)
                self.assertIn("timed out", timed.stderr)
            finally:
                holder.terminate()
                holder.wait(timeout=2)

    def test_development_image_preserves_rustup_path_and_numeric_user(self):
        image = (ROOT / "docker/development.Dockerfile").read_text()
        self.assertIn("/usr/local/cargo/bin", image)
        self.assertIn("USER ${DEV_UID}:${DEV_GID}", image)
        self.assertIn("getent group", image)

    def test_development_image_warms_corepack_as_runtime_user(self):
        image = (ROOT / "docker/development.Dockerfile").read_text()
        self.assertGreater(image.rfind('chown -R "${DEV_UID}:${DEV_GID}" /home/developer'), image.find("corepack prepare"))
        self.assertGreater(image.find("RUN pnpm --version"), image.find("USER ${DEV_UID}:${DEV_GID}"))
