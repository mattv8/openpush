import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
HELPER = ROOT / "infra/dev/desktop.sh"


class DesktopHelperTests(unittest.TestCase):
    def run_helper(self, *args, env=None, cwd=ROOT):
        values = os.environ.copy()
        values.update(env or {})
        return subprocess.run(
            ["bash", str(HELPER), *args],
            cwd=cwd,
            env=values,
            text=True,
            capture_output=True,
        )

    def fake_command(self, directory, name, body):
        path = pathlib.Path(directory) / name
        path.write_text("#!/usr/bin/env bash\nset -eu\n" + body)
        path.chmod(0o755)
        return path

    def test_rejects_unknown_action_without_running_tools(self):
        result = self.run_helper("unknown")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Usage:", result.stderr)

    def test_wsl_passes_fixed_windows_arguments_without_shell_injection(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            arguments = fake_bin / "arguments"
            self.fake_command(fake_bin, "uname", "echo Linux")
            self.fake_command(fake_bin, "wslpath", "echo 'C:\\Users\\テスト Space\\OpenPush'")
            self.fake_command(fake_bin, "powershell.exe", f"printf '%s\\n' \"$@\" > '{arguments}'")
            result = self.run_helper(
                "build",
                env={"WSL_INTEROP": "1", "PATH": f"{fake_bin}:{os.environ['PATH']}"},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                arguments.read_text().splitlines(),
                [
                    "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                    "C:\\Users\\テスト Space\\OpenPush\\infra\\dev\\windows-desktop.ps1",
                    "-Action", "build", "-RepoPath", "C:\\Users\\テスト Space\\OpenPush",
                    "-CargoTargetDir", "C:\\Users\\テスト Space\\OpenPush",
                ],
            )

    def test_wsl_rejects_unc_checkout_before_starting_powershell(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            called = fake_bin / "called"
            self.fake_command(fake_bin, "uname", "echo Linux")
            self.fake_command(fake_bin, "wslpath", "echo '\\\\wsl.localhost\\Ubuntu\\home\\openpush'")
            self.fake_command(fake_bin, "powershell.exe", f"touch '{called}'")
            result = self.run_helper(
                "build",
                env={"WSL_INTEROP": "1", "PATH": f"{fake_bin}:{os.environ['PATH']}"},
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("drive-letter NTFS", result.stderr)
            self.assertFalse(called.exists())

    def test_macos_changes_to_repository_before_version_probe(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            probe_cwd = fake_bin / "probe-cwd"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", f"pwd > '{probe_cwd}'; echo v24.21.0")
            self.fake_command(fake_bin, "pnpm", "if [ \"${1:-}\" = --version ]; then echo 12.8.1; fi")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            result = self.run_helper(
                "dev",
                env={"PATH": f"{fake_bin}:{os.environ['PATH']}"}, cwd=fake_bin,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(probe_cwd.read_text().strip(), str(ROOT))


if __name__ == "__main__":
    unittest.main()
