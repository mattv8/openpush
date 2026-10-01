import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("demo", ROOT / "infra/dev/demo.py")
demo = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = demo
SPEC.loader.exec_module(demo)


class DemoTests(unittest.TestCase):
    def test_private_run_files_and_synthetic_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            run = demo.make_run_directory(Path(directory))
            self.assertEqual((run.stat().st_mode & 0o777), 0o700)
            env_file = demo.write_environment(run, 43123)
            self.assertEqual((env_file.stat().st_mode & 0o777), 0o600)
            values = env_file.read_text()
            self.assertIn("POSTGRES_PASSWORD=synthetic-", values)
            self.assertNotIn("DATABASE_URL", values)

    def test_pipe_reader_handles_eof_without_a_newline(self):
        process = subprocess.Popen(
            [sys.executable, "-c", "import sys; sys.stdout.write('ready')"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        with tempfile.TemporaryDirectory() as directory:
            reader = demo.ProcessOutput(process, Path(directory) / "log")
            self.assertTrue(reader.wait_for("ready", "stdout", 2))
            process.wait(timeout=2)
            reader.close()

    def test_pipe_reader_requires_marker_after_checkpoint(self):
        process = subprocess.Popen(
            [sys.executable, "-c", "import sys; print('phase=sync'); sys.stdout.flush(); sys.stdin.readline(); print('phase=sync'); sys.stdout.flush()"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        with tempfile.TemporaryDirectory() as directory:
            reader = demo.ProcessOutput(process, Path(directory) / "log")
            self.assertTrue(reader.wait_for("phase=sync", "stdout", 2))
            checkpoint = reader.checkpoint("stdout")
            process.stdin.write(b"continue\n")
            process.stdin.flush()
            self.assertTrue(reader.wait_for("phase=sync", "stdout", 2, after=checkpoint))
            self.assertEqual(process.wait(timeout=2), 0)
            process.stdin.close()
            reader.close()

    def test_sanitized_environment_removes_synthetic_and_compose_overrides(self):
        values = demo.synthetic_values(43123)
        cleaned = demo.sanitized_environment({"PATH": "/bin", "BIND_ADDR": "bad", "COMPOSE_PROJECT_NAME": "bad", "OTHER": "ok"}, values)
        self.assertEqual(cleaned, {"PATH": "/bin", "OTHER": "ok"})

    def test_host_forwards_cargo_timeout_to_isolated_runner(self):
        calls = []
        original_checked, original_cleanup, original_root = demo.checked, demo.subprocess.run, demo.ROOT
        with tempfile.TemporaryDirectory() as directory:
            try:
                demo.ROOT = Path(directory)
                demo.checked = lambda command, **kwargs: calls.append(command)
                demo.subprocess.run = lambda *args, **kwargs: None
                previous = os.environ.get("OPENPUSH_DEMO_CARGO_TIMEOUT")
                os.environ["OPENPUSH_DEMO_CARGO_TIMEOUT"] = "17"
                demo.run_host_demo()
            finally:
                if previous is None:
                    os.environ.pop("OPENPUSH_DEMO_CARGO_TIMEOUT", None)
                else:
                    os.environ["OPENPUSH_DEMO_CARGO_TIMEOUT"] = previous
                demo.checked, demo.subprocess.run, demo.ROOT = original_checked, original_cleanup, original_root
        run = next(command for command in calls if "run" in command)
        self.assertIn("OPENPUSH_DEMO_CARGO_TIMEOUT=17", run)

    def test_create_owner_uses_configured_target_binary(self):
        source = (ROOT / "infra/dev/demo.py").read_text()
        self.assertIn('[server_binary, "create-owner"]', source)

    def test_migration_uses_built_server_binary_not_cargo_run(self):
        source = (ROOT / "infra/dev/demo.py").read_text()
        self.assertIn('checked([server_binary, "migrate"], env=server_env)', source)
        self.assertNotIn('"-p", "openpush-server", "--", "migrate"', source)

    def test_credential_metadata_is_secret_free_for_invalid_state(self):
        with tempfile.TemporaryDirectory() as directory:
            credentials = Path(directory) / "credentials.json"
            canary = "NEVER-LOG-DEVICE-TOKEN"
            credentials.write_text(json.dumps({"version": "wrong", "origin": "http://bad", "vaultId": "vault", "deviceId": "device", "deviceToken": canary}))
            credentials.chmod(0o600)
            metadata, valid = demo.credential_metadata(credentials)
            rendered = json.dumps(metadata)
            self.assertFalse(valid)
            self.assertNotIn(canary, rendered)
            self.assertNotIn("http://bad", rendered)
            self.assertEqual(metadata["type"], "regular")
            self.assertFalse(metadata["version_valid"])

    def test_host_demo_down_is_scoped_on_success_and_failure(self):
        for failing in (False, True):
            calls = []
            original_checked, original_cleanup, original_root = demo.checked, demo.subprocess.run, demo.ROOT
            with tempfile.TemporaryDirectory() as directory:
                try:
                    demo.ROOT = Path(directory)
                    def checked(command, **kwargs):
                        calls.append(command)
                        if failing and command[-2:] == ["build", "dev"]:
                            raise RuntimeError("build failed")
                    demo.checked = checked
                    demo.subprocess.run = lambda command, **kwargs: calls.append(command)
                    if failing:
                        with self.assertRaises(RuntimeError):
                            demo.run_host_demo()
                    else:
                        demo.run_host_demo()
                finally:
                    demo.checked, demo.subprocess.run, demo.ROOT = original_checked, original_cleanup, original_root
            down = next(command for command in calls if command[-3:] == ["down", "--volumes", "--remove-orphans"])
            self.assertIn("--project-name", down)
            self.assertTrue(down[down.index("--project-name") + 1].startswith("openpush-demo-"))
            self.assertNotIn("openpush", down)

    def test_timeout_rejects_nonpositive_and_non_numeric_values(self):
        previous = os.environ.get("DEMO_TEST_TIMEOUT")
        try:
            for value in ("0", "-1", "abc"):
                os.environ["DEMO_TEST_TIMEOUT"] = value
                with self.assertRaises(SystemExit):
                    demo.timeout_from_environment("DEMO_TEST_TIMEOUT", 1)
        finally:
            if previous is None:
                os.environ.pop("DEMO_TEST_TIMEOUT", None)
            else:
                os.environ["DEMO_TEST_TIMEOUT"] = previous

    def test_host_demo_installs_interrupt_handlers_for_cleanup(self):
        handlers, calls = {}, []
        original_checked, original_cleanup, original_root, original_signal = demo.checked, demo.subprocess.run, demo.ROOT, demo.signal.signal
        with tempfile.TemporaryDirectory() as directory:
            try:
                demo.ROOT = Path(directory)
                demo.checked = lambda command, **kwargs: calls.append(command)
                demo.subprocess.run = lambda *args, **kwargs: None
                def signal_stub(signum, handler):
                    handlers.setdefault(signum, handler)
                    return demo.signal.SIG_DFL
                demo.signal.signal = signal_stub
                demo.run_host_demo()
            finally:
                demo.checked, demo.subprocess.run, demo.ROOT, demo.signal.signal = original_checked, original_cleanup, original_root, original_signal
        self.assertIn(demo.signal.SIGHUP, handlers)
        with self.assertRaises(KeyboardInterrupt):
            handlers[demo.signal.SIGHUP](demo.signal.SIGHUP, None)

    def test_child_cleanup_terminates_owned_child(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        controller = demo.ChildController()
        controller.add(child)
        controller.cleanup()
        self.assertIsNotNone(child.wait(timeout=2))
