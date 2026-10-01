import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]


class AndroidHelperTests(unittest.TestCase):
    def run_helper(self, *args, env=None):
        return self.run_script(ROOT / "infra/dev/android.sh", *args, env=env)

    def run_script(self, script, *args, env=None):
        values = os.environ.copy()
        values.update(env or {})
        return subprocess.run(
            ["bash", str(script), *args],
            cwd=script.parents[2], env=values, text=True, capture_output=True,
        )

    def test_rejects_unknown_command(self):
        result = self.run_helper("nope")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Usage:", result.stderr)

    def test_deploy_refuses_physical_serial(self):
        result = self.run_helper("deploy", env={"OPENPUSH_ANDROID_SERIAL": "R58N123"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("emulator", result.stderr.lower())

    def test_container_runner_rejects_unknown_command(self):
        result = subprocess.run(
            ["bash", str(ROOT / "infra/dev/android-container-run.sh"), "nope"],
            cwd=ROOT, text=True, capture_output=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Usage:", result.stderr)

    def test_native_verifier_defaults_to_both_abis(self):
        content = (ROOT / "infra/compose/verify-android-native.sh").read_text()
        self.assertIn("aarch64-linux-android", content)
        self.assertIn("x86_64-linux-android", content)
        self.assertIn("CARGO_TARGET_DIR", content)

    def test_wsl_crlf_emulator_smoke_uses_windows_apk_path_and_requires_tests(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp)
            bin_dir = temp / "bin"
            sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True)
            bin_dir.mkdir()
            log = temp / "adb.log"
            adb = sdk / "platform-tools" / "adb.exe"
            adb.write_text(
                "#!/bin/sh\n"
                "echo \"$@\" >> \"$ADB_LOG\"\n"
                "case \"$*\" in *devices*) printf 'List of devices attached\\nemulator-5554\\tdevice\\r\\n' ;; *getprop*) echo x86_64 ;; *instrument*) printf 'OK (2 tests)\\r\\nINSTRUMENTATION_CODE: -1\\r\\n' ;; esac\n"
            )
            wslpath = bin_dir / "wslpath"
            wslpath.write_text("#!/bin/sh\n[ \"$1\" = -u ] && echo \"$FAKE_SDK\" || echo \"WIN:$2\"\n")
            powershell = bin_dir / "powershell.exe"
            powershell.write_text("#!/bin/sh\nexit 0\n")
            curl = bin_dir / "curl"
            curl.write_text("#!/bin/sh\nexit 0\n")
            for path in (adb, wslpath, powershell, curl):
                path.chmod(0o755)
            artifacts = temp / "artifact path"
            artifacts.mkdir()
            (artifacts / "app-debug.apk").touch()
            (artifacts / "app-debug-androidTest.apk").touch()
            result = self.run_helper(
                "smoke",
                env={
                    "WSL_INTEROP": "1", "ANDROID_SDK_ROOT": r"C:\\Sdk", "FAKE_SDK": str(sdk),
                    "OPENPUSH_ANDROID_ARTIFACTS": str(artifacts), "ADB_LOG": str(log),
                    "PATH": f"{bin_dir}:{os.environ['PATH']}",
                },
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("install -r WIN:", log.read_text())

    def test_smoke_rejects_zero_instrumentation_tests(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp)
            sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\ncase \"$*\" in *devices*) echo 'emulator-5554 device' ;; *getprop*) echo x86_64 ;; *instrument*) echo 'OK (0 tests)' ;; esac\n")
            adb.chmod(0o755)
            artifacts = temp / "artifacts"; artifacts.mkdir()
            (artifacts / "app-debug.apk").touch(); (artifacts / "app-debug-androidTest.apk").touch()
            result = self.run_helper("smoke", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_ARTIFACTS": str(artifacts)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("instrumentation smoke failed", result.stderr)

    def test_smoke_accepts_android_success_code_minus_one(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\ncase \"$*\" in *devices*) echo 'emulator-5554 device' ;; *getprop*) echo x86_64 ;; *instrument*) printf 'OK (1 test)\\nINSTRUMENTATION_CODE: -1\\n' ;; esac\n")
            adb.chmod(0o755)
            artifacts = temp / "artifacts"; artifacts.mkdir()
            (artifacts / "app-debug.apk").touch(); (artifacts / "app-debug-androidTest.apk").touch()
            result = self.run_helper("smoke", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_ARTIFACTS": str(artifacts)})
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_wsl_health_uses_windows_powershell_not_linux_curl(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); bin_dir = temp / "bin"; sdk = temp / "sdk"
            bin_dir.mkdir(); (sdk / "platform-tools").mkdir(parents=True)
            log = temp / "powershell.log"
            adb = sdk / "platform-tools" / "adb.exe"
            adb.write_text("#!/bin/sh\ncase \"$*\" in *devices*) echo 'emulator-5554 device' ;; *getprop*) echo x86_64 ;; esac\n")
            wslpath = bin_dir / "wslpath"; wslpath.write_text("#!/bin/sh\n[ \"$1\" = -u ] && echo \"$FAKE_SDK\" || echo \"WIN:$2\"\n")
            powershell = bin_dir / "powershell.exe"; powershell.write_text("#!/bin/sh\necho \"$@\" > \"$POWERSHELL_LOG\"\nprintf '%s' \"$WSLENV\" | grep -q 'OPENPUSH_HEALTH_URL/w'\n")
            curl = bin_dir / "curl"; curl.write_text("#!/bin/sh\nexit 99\n")
            for path in (adb, wslpath, powershell, curl): path.chmod(0o755)
            artifacts = temp / "artifacts"; artifacts.mkdir(); (artifacts / "app-debug.apk").touch()
            result = self.run_helper("deploy", env={"WSL_INTEROP": "1", "ANDROID_SDK_ROOT": r"C:\\Sdk", "FAKE_SDK": str(sdk), "POWERSHELL_LOG": str(log), "OPENPUSH_ANDROID_ARTIFACTS": str(artifacts), "PATH": f"{bin_dir}:{os.environ['PATH']}"})
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Invoke-WebRequest", log.read_text())

    def test_emulator_timeout_terminates_only_started_process(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; log = temp / "emulator.log"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\n[ \"$1\" = devices ] && echo 'List of devices attached'\n")
            emulator = sdk / "emulator" / "emulator"
            emulator.write_text("#!/bin/sh\nif [ \"$1\" = -list-avds ]; then echo test-avd; exit; fi\necho started >> \"$EMULATOR_LOG\"\ntrap 'echo stopped >> \"$EMULATOR_LOG\"; exit' TERM\nsleep 30 & wait\n")
            adb.chmod(0o755); emulator.chmod(0o755)
            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_AVD": "test-avd", "OPENPUSH_ANDROID_BOOT_TIMEOUT": "1", "EMULATOR_LOG": str(log), "OPENPUSH_ANDROID_ARTIFACTS": str(temp / "artifacts")})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("did not appear", result.stderr)
            self.assertNotIn(".opencode", result.stderr)
            self.assertIn("stopped", log.read_text())

    def test_emulator_timeout_never_kills_preexisting_other_avd(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; log = temp / "adb.log"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\necho \"$@\" >> \"$ADB_LOG\"\ncase \"$*\" in *devices*) echo 'emulator-5554 device' ;; *ro.boot.qemu.avd_name*) echo other-avd ;; esac\n")
            emulator = sdk / "emulator" / "emulator"
            emulator.write_text("#!/bin/sh\n[ \"$1\" = -list-avds ] && { echo test-avd; exit; }\ntrap 'exit' TERM\nsleep 30 & wait\n")
            adb.chmod(0o755); emulator.chmod(0o755)
            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_AVD": "test-avd", "OPENPUSH_ANDROID_BOOT_TIMEOUT": "1", "OPENPUSH_ANDROID_ARTIFACTS": str(temp / "artifacts"), "ADB_LOG": str(log)})
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn("-s emulator-5554 emu kill", log.read_text())

    def test_host_tool_missing_is_actionable(self):
        with tempfile.TemporaryDirectory() as temp:
            result = self.run_helper("deploy", env={"ANDROID_SDK_ROOT": temp})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Android SDK tool is missing", result.stderr)

    def test_host_build_prepares_android_cache_volumes(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); bin_dir = temp / "bin"; bin_dir.mkdir(); log = temp / "docker.log"
            repo = temp / "repo"; script = repo / "infra/dev/android.sh"
            script.parent.mkdir(parents=True); shutil.copy(ROOT / "infra/dev/android.sh", script)
            (repo / ".env").write_text("synthetic=1\n")
            docker = bin_dir / "docker"
            docker.write_text("#!/bin/sh\necho \"$@\" >> \"$DOCKER_LOG\"\nexit 0\n")
            docker.chmod(0o755)
            env = {"PATH": f"{bin_dir}:{os.environ['PATH']}", "DOCKER_LOG": str(log), "OPENPUSH_ANDROID_ARTIFACTS": str(temp / "artifacts")}
            result = self.run_script(script, "build", env=env)
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text()
            for volume in ("android-sdk", "android-gradle", "android-cargo", "android-target", "android-debug-keystore"):
                self.assertIn(f"volume create openpush-{volume}-", calls)
            self.assertIn("run --build --rm android run build", calls)

    def test_host_build_missing_env_has_setup_hint(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); bin_dir = temp / "bin"; bin_dir.mkdir()
            repo = temp / "repo"; script = repo / "infra/dev/android.sh"
            script.parent.mkdir(parents=True); shutil.copy(ROOT / "infra/dev/android.sh", script)
            docker = bin_dir / "docker"; docker.write_text("#!/bin/sh\nexit 0\n"); docker.chmod(0o755)
            result = self.run_script(script, "build", env={"PATH": f"{bin_dir}:{os.environ['PATH']}", "OPENPUSH_ANDROID_ARTIFACTS": str(temp / "artifacts")})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("dev-setup", result.stderr)
