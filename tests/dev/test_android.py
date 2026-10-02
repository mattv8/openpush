import os
import pathlib
import pty
import select
import signal
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]


class AndroidHelperTests(unittest.TestCase):
    def run_helper(self, *args, env=None):
        return self.run_script(ROOT / "infra/dev/android.sh", *args, env=env)

    def run_script(self, script, *args, env=None):
        values = os.environ.copy()
        values.pop("OPENPUSH_ANDROID_AVD", None)
        values.pop("OPENPUSH_ANDROID_SERIAL", None)
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

    def test_emulator_reuses_sole_running_avd_without_local_metadata_or_spawn(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; log = temp / "tools.log"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\necho \"$@\" >> \"$TOOLS_LOG\"\ncase \"$*\" in *devices*) echo 'emulator-5554 device' ;; *ro.boot.qemu.avd_name*) echo remote-avd ;; *sys.boot_completed*) echo 1 ;; esac\n")
            emulator = sdk / "emulator" / "emulator"
            emulator.write_text("#!/bin/sh\necho \"emulator $@\" >> \"$TOOLS_LOG\"\n")
            adb.chmod(0o755); emulator.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk), "TOOLS_LOG": str(log)})
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text()
            self.assertNotIn("emulator -list-avds", calls)
            self.assertNotIn("emulator -avd", calls)
            self.assertNotIn("emu kill", calls)

    def test_emulator_reuses_explicit_running_serial_without_local_avd_metadata(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; log = temp / "tools.log"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\necho \"$@\" >> \"$TOOLS_LOG\"\ncase \"$*\" in *devices*) echo 'emulator-5554 device' ;; *ro.boot.qemu.avd_name*) echo remote-avd ;; *sys.boot_completed*) echo 1 ;; esac\n")
            emulator = sdk / "emulator" / "emulator"
            emulator.write_text("#!/bin/sh\necho \"emulator $@\" >> \"$TOOLS_LOG\"\n")
            adb.chmod(0o755); emulator.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_SERIAL": "emulator-5554", "TOOLS_LOG": str(log)})
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("emulator -list-avds", log.read_text())

    def test_emulator_rejects_empty_reported_running_avd_name(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\ncase \"$*\" in *devices*) echo 'emulator-5554 device' ;; *ro.boot.qemu.avd_name*) true ;; esac\n")
            adb.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("did not report an AVD name", result.stderr)

    def test_emulator_rejects_physical_explicit_serial(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\n[ \"$1\" = devices ] && true\n")
            adb.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_SERIAL": "physical-123"})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("refusing physical device", result.stderr)

    def test_emulator_refuses_ambiguous_running_emulators_without_avd_override(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\n[ \"$1\" = devices ] && printf 'emulator-5554 device\\nemulator-5556 device\\n'\n")
            adb.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("OPENPUSH_ANDROID_SERIAL", result.stderr)

    def test_emulator_starts_sole_configured_avd_when_none_is_running(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; state = temp / "started"; log = temp / "tools.log"; pid_path = temp / "emulator.pid"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\necho \"$@\" >> \"$TOOLS_LOG\"\ncase \"$*\" in *devices*) [ -f \"$STARTED\" ] && echo 'emulator-5554 device' ;; *ro.boot.qemu.avd_name*) echo sole-avd ;; *sys.boot_completed*) echo 1 ;; esac\n")
            emulator = sdk / "emulator" / "emulator"
            emulator.write_text("#!/bin/sh\n[ \"$1\" = -list-avds ] && { echo sole-avd; exit; }\necho \"$@\" >> \"$TOOLS_LOG\"\necho \"$$\" > \"$EMULATOR_PID\"\ntouch \"$STARTED\"\nexec sleep 30\n")
            adb.chmod(0o755); emulator.chmod(0o755)

            try:
                result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk), "STARTED": str(state), "TOOLS_LOG": str(log), "EMULATOR_PID": str(pid_path), "OPENPUSH_ANDROID_ARTIFACTS": str(temp / "artifacts")})
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("-avd sole-avd", log.read_text())
            finally:
                if pid_path.exists():
                    try:
                        os.kill(int(pid_path.read_text()), 15)
                    except ProcessLookupError:
                        pass

    def test_emulator_survives_controlling_terminal_close_after_ready(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; state = temp / "started"; pid_path = temp / "emulator.pid"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"
            adb.write_text(
                "#!/bin/sh\ncase \"$*\" in\n"
                "  *devices*) [ -f \"$STARTED\" ] && echo 'emulator-5554 device' ;;\n"
                "  *ro.boot.qemu.avd_name*) echo test-avd ;;\n"
                "  *sys.boot_completed*) echo 1 ;;\nesac\n"
            )
            emulator = sdk / "emulator" / "emulator"
            emulator.write_text(
                f"#!{sys.executable}\n"
                "import os, signal, sys\n"
                "if sys.argv[1:] == ['-list-avds']:\n    print('test-avd')\n    raise SystemExit\n"
                "open(os.environ['EMULATOR_PID'], 'w').write(str(os.getpid()))\n"
                "open(os.environ['STARTED'], 'w').close()\n"
                "def raise_(signum):\n    raise SystemExit(128 + signum)\n"
                "signal.signal(signal.SIGHUP, lambda *_: raise_(signal.SIGHUP))\n"
                "def wait():\n    signal.pause(); wait()\n"
                "wait()\n"
            )
            adb.chmod(0o755); emulator.chmod(0o755)
            env = os.environ.copy()
            env.pop("OPENPUSH_ANDROID_SERIAL", None)
            env.update({"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_AVD": "test-avd", "OPENPUSH_ANDROID_BOOT_TIMEOUT": "5", "OPENPUSH_ANDROID_ARTIFACTS": str(temp / "artifacts"), "STARTED": str(state), "EMULATOR_PID": str(pid_path)})
            child, terminal = pty.fork()
            if child == 0:
                os.execvpe("bash", ["bash", str(ROOT / "infra/dev/android.sh"), "emulator"], env)
            output = ""
            try:
                while "Android emulator ready:" not in output:
                    readable, _, _ = select.select([terminal], [], [], 5)
                    self.assertTrue(readable, "helper did not report emulator readiness")
                    output += os.read(terminal, 4096).decode(errors="replace")
                self.assertTrue(pid_path.exists())
                emulator_pid = int(pid_path.read_text())
                os.close(terminal)
                terminal = None
                select.select([], [], [], 0.2)
                os.kill(emulator_pid, 0)
            finally:
                if terminal is not None:
                    os.close(terminal)
                if pid_path.exists():
                    try:
                        os.kill(int(pid_path.read_text()), signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                os.waitpid(child, 0)

    def test_wsl_timeout_uses_native_identity_helper_without_killing_serial(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; tools = temp / "bin"
            state = temp / "started"; adb_log = temp / "adb.log"; process_log = temp / "process.log"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir(); tools.mkdir()
            adb = sdk / "platform-tools" / "adb.exe"
            adb.write_text("#!/bin/sh\necho \"$@\" >> \"$ADB_LOG\"\ncase \"$*\" in *devices*) [ -f \"$STARTED\" ] && echo 'emulator-5556 device' ;; *ro.boot.qemu.avd_name*) echo target-avd ;; *sys.boot_completed*) echo 0 ;; esac\n")
            emulator = sdk / "emulator" / "emulator.exe"
            emulator.write_text("#!/bin/sh\n[ \"$1\" = -list-avds ] && echo target-avd\n")
            wslpath = tools / "wslpath"
            wslpath.write_text("#!/bin/sh\n[ \"$1\" = -u ] && echo \"$FAKE_SDK\" || echo \"WIN:$2\"\n")
            powershell = tools / "powershell.exe"
            powershell.write_text("#!/bin/sh\necho \"$@\" >> \"$PROCESS_LOG\"\ncase \"$*\" in *'-Action start'*) touch \"$STARTED\"; echo '123|456' ;; esac\n")
            for path in (adb, emulator, wslpath, powershell): path.chmod(0o755)

            result = self.run_helper("emulator", env={
                "WSL_INTEROP": "1", "ANDROID_SDK_ROOT": r"C:\\Sdk", "FAKE_SDK": str(sdk),
                "OPENPUSH_ANDROID_AVD": "target-avd", "OPENPUSH_ANDROID_BOOT_TIMEOUT": "1",
                "OPENPUSH_ANDROID_ARTIFACTS": str(temp / "artifacts"), "STARTED": str(state),
                "ADB_LOG": str(adb_log), "PROCESS_LOG": str(process_log),
                "PATH": f"{tools}:{os.environ['PATH']}",
            })
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn("emu kill", adb_log.read_text())
            self.assertIn("-Action stop", process_log.read_text())
            self.assertIn("-ProcessId 123 -StartedAt 456", process_log.read_text())

    def test_emulator_requires_one_configured_avd_when_none_is_running(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"; adb.write_text("#!/bin/sh\n[ \"$1\" = devices ] && true\n")
            emulator = sdk / "emulator" / "emulator"; emulator.write_text("#!/bin/sh\n[ \"$1\" = -list-avds ] && exit\n")
            adb.chmod(0o755); emulator.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("OPENPUSH_ANDROID_AVD", result.stderr)

    def test_emulator_refuses_ambiguous_configured_avds_when_none_is_running(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"; adb.write_text("#!/bin/sh\n[ \"$1\" = devices ] && true\n")
            emulator = sdk / "emulator" / "emulator"; emulator.write_text("#!/bin/sh\n[ \"$1\" = -list-avds ] && printf 'first\\nsecond\\n'\n")
            adb.chmod(0o755); emulator.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("OPENPUSH_ANDROID_AVD", result.stderr)

    def test_emulator_rejects_unknown_explicit_avd_when_not_running(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"; adb.write_text("#!/bin/sh\n[ \"$1\" = devices ] && true\n")
            emulator = sdk / "emulator" / "emulator"; emulator.write_text("#!/bin/sh\n[ \"$1\" = -list-avds ] && echo another-avd\n")
            adb.chmod(0o755); emulator.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_AVD": "missing-avd"})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("does not exist", result.stderr)

    def test_emulator_exited_launcher_never_claims_or_kills_new_same_avd_serial(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; state = temp / "started"; count_path = temp / "adb-count"; fifo = temp / "exit"; done_fifo = temp / "exit-done"; exited = temp / "exited"; log = temp / "adb.log"
            os.mkfifo(fifo)
            os.mkfifo(done_fifo)
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"
            adb.write_text(
                "#!/bin/sh\necho \"$@\" >> \"$ADB_LOG\"\ncount=0\n[ -f \"$ADB_COUNT\" ] && count=$(cat \"$ADB_COUNT\")\ncount=$((count + 1))\necho \"$count\" > \"$ADB_COUNT\"\n"
                "case \"$*\" in\n"
                "  *devices*) if [ \"$count\" -ge 4 ]; then echo 'emulator-5556 device'; else echo 'emulator-5554 offline'; fi ;;\n"
                "  *ro.boot.qemu.avd_name*) printf x > \"$EXIT_FIFO\"; read ignored < \"$EXIT_DONE_FIFO\"; echo race-avd ;;\n"
                "esac\n"
            )
            emulator = sdk / "emulator" / "emulator"
            emulator.write_text("#!/bin/sh\n[ \"$1\" = -list-avds ] && { echo race-avd; exit; }\ntouch \"$STARTED\"\nread ignored < \"$EXIT_FIFO\"\ntouch \"$EXITED\"\nprintf x > \"$EXIT_DONE_FIFO\"\n")
            adb.chmod(0o755); emulator.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk), "STARTED": str(state), "ADB_COUNT": str(count_path), "EXIT_FIFO": str(fifo), "EXIT_DONE_FIFO": str(done_fifo), "EXITED": str(exited), "ADB_LOG": str(log), "OPENPUSH_ANDROID_ARTIFACTS": str(temp / "artifacts"), "OPENPUSH_ANDROID_BOOT_TIMEOUT": "2"})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("process exited", result.stderr)
            self.assertNotIn("emu kill", log.read_text())

    def test_emulator_never_claims_or_kills_preexisting_offline_serial(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; log = temp / "adb.log"; state = temp / "started"; count_path = temp / "adb-count"; ready = temp / "ready"
            os.mkfifo(ready)
            (sdk / "platform-tools").mkdir(parents=True); (sdk / "emulator").mkdir()
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\necho \"$@\" >> \"$ADB_LOG\"\ncount=0\n[ -f \"$ADB_COUNT\" ] && count=$(cat \"$ADB_COUNT\")\ncount=$((count + 1))\necho \"$count\" > \"$ADB_COUNT\"\ncase \"$*\" in *devices*) if [ \"$count\" -eq 4 ]; then read ready < \"$READY_FIFO\"; fi; if [ -f \"$STARTED\" ]; then echo 'emulator-5554 device'; else echo 'emulator-5554 offline'; fi ;; *ro.boot.qemu.avd_name*) echo test-avd ;; *sys.boot_completed*) echo 0 ;; esac\n")
            emulator = sdk / "emulator" / "emulator"
            emulator.write_text("#!/bin/sh\n[ \"$1\" = -list-avds ] && { echo test-avd; exit; }\ntouch \"$STARTED\"\nprintf 'ready\\n' > \"$READY_FIFO\"\nexec sleep 30\n")
            adb.chmod(0o755); emulator.chmod(0o755)

            result = self.run_helper("emulator", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_BOOT_TIMEOUT": "1", "OPENPUSH_ANDROID_ARTIFACTS": str(temp / "artifacts"), "ADB_LOG": str(log), "STARTED": str(state), "ADB_COUNT": str(count_path), "READY_FIFO": str(ready)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("did not appear", result.stderr)
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

    def test_open_launches_selected_emulator_after_abi_and_reverse(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; log = temp / "adb.log"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\necho \"$@\" >> \"$ADB_LOG\"\ncase \"$*\" in *devices*) echo 'emulator-5556 device';; *getprop*) echo x86_64;; *'am start'*) printf 'Status: ok\\r\\nActivity: dev.openpush.mobile/.MainActivity\\r\\n';; esac\n")
            adb.chmod(0o755)
            result = self.run_helper("open", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_SERIAL": "emulator-5556", "ADB_LOG": str(log)})
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text()
            self.assertIn("reverse tcp:7000 tcp:7000", calls)
            self.assertIn("shell am start -W -n dev.openpush.mobile/.MainActivity", calls)

    def test_open_refuses_am_error_even_when_adb_exits_zero(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\ncase \"$*\" in *devices*) echo 'emulator-5554 device';; *getprop*) echo arm64-v8a;; *'am start'*) printf 'Error: Type 3\\n';; esac\n")
            adb.chmod(0o755)
            result = self.run_helper("open", env={"ANDROID_SDK_ROOT": str(sdk)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("launch failed", result.stderr)

    def test_open_refuses_nonzero_adb_start(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\ncase \"$*\" in *devices*) echo 'emulator-5554 device';; *getprop*) echo x86_64;; *'am start'*) echo launch-output; exit 1;; esac\n")
            adb.chmod(0o755)
            result = self.run_helper("open", env={"ANDROID_SDK_ROOT": str(sdk)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("launch failed", result.stderr)

    def test_open_refuses_missing_success_status_when_adb_exits_zero(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\ncase \"$*\" in *devices*) echo 'emulator-5554 device';; *getprop*) echo x86_64;; *'am start'*) true;; esac\n")
            adb.chmod(0o755)
            result = self.run_helper("open", env={"ANDROID_SDK_ROOT": str(sdk)})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("launch failed", result.stderr)

    def test_open_rejects_unsupported_abi_before_reverse_or_launch(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; log = temp / "adb.log"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"
            adb.write_text("#!/bin/sh\necho \"$@\" >> \"$ADB_LOG\"\ncase \"$*\" in *devices*) echo 'emulator-5554 device';; *getprop*) echo armeabi-v7a;; esac\n")
            adb.chmod(0o755)
            result = self.run_helper("open", env={"ANDROID_SDK_ROOT": str(sdk), "ADB_LOG": str(log)})
            self.assertNotEqual(result.returncode, 0)
            calls = log.read_text()
            self.assertNotIn("reverse", calls)
            self.assertNotIn("am start", calls)

    def test_open_refuses_physical_serial_before_calling_adb(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = pathlib.Path(temp); sdk = temp / "sdk"; log = temp / "adb.log"
            (sdk / "platform-tools").mkdir(parents=True)
            adb = sdk / "platform-tools" / "adb"; adb.write_text("#!/bin/sh\necho called >> \"$ADB_LOG\"\n") ; adb.chmod(0o755)
            result = self.run_helper("open", env={"ANDROID_SDK_ROOT": str(sdk), "OPENPUSH_ANDROID_SERIAL": "device-1", "ADB_LOG": str(log)})
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(log.exists())
