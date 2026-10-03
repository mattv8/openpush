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
        values.pop("PEPPY_MACOS_SIGNING_IDENTITY", None)
        values.pop("APPLE_SIGNING_IDENTITY", None)
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
            self.fake_command(fake_bin, "wslpath", "echo 'C:\\Users\\テスト Space\\Peppy'")
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
                    "C:\\Users\\テスト Space\\Peppy\\infra\\dev\\windows-desktop.ps1",
                    "-Action", "build", "-RepoPath", "C:\\Users\\テスト Space\\Peppy",
                    "-CargoTargetDir", "C:\\Users\\テスト Space\\Peppy",
                ],
            )

    def test_wsl_rejects_unc_checkout_before_starting_powershell(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            called = fake_bin / "called"
            self.fake_command(fake_bin, "uname", "echo Linux")
            self.fake_command(fake_bin, "wslpath", "echo '\\\\wsl.localhost\\Ubuntu\\home\\peppy'")
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

    def test_macos_open_starts_the_current_bundle_as_a_new_instance(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary) / "bin"
            fake_bin.mkdir()
            target = pathlib.Path(temporary) / "target"
            bundle = target / "release/bundle/macos/Peppy.app"
            bundle.mkdir(parents=True)
            arguments = pathlib.Path(temporary) / "open-arguments"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "open", f"printf '%s\\n' \"$@\" > '{arguments}'")

            result = self.run_helper("open", env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "CARGO_TARGET_DIR": str(target)})
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(arguments.read_text().splitlines(), ["-n", str(bundle)])

    def test_macos_uses_pinned_homebrew_tools_when_path_versions_mismatch(self):
        with tempfile.TemporaryDirectory() as temporary:
            temporary_path = pathlib.Path(temporary)
            fake_bin = temporary_path / "bin"
            node_prefix = temporary_path / "node"
            rust_prefix = temporary_path / "rust"
            fake_bin.mkdir()
            (node_prefix / "bin").mkdir(parents=True)
            (rust_prefix / "bin").mkdir(parents=True)
            selected = temporary_path / "selected"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v20.0.0")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.0.0 (test)'")
            self.fake_command(fake_bin, "pnpm", f"if [ \"${{1:-}}\" = --version ]; then echo 12.8.1; else command -v node > '{selected}'; fi")
            self.fake_command(fake_bin, "brew", f"case \"$2\" in node@24) echo '{node_prefix}' ;; rustup) echo '{rust_prefix}' ;; esac")
            self.fake_command(node_prefix / "bin", "node", "echo v24.21.0")
            self.fake_command(rust_prefix / "bin", "rustc", "echo 'rustc 1.98.1 (test)'")

            result = self.run_helper("dev", env={"PATH": f"{fake_bin}:{os.environ['PATH']}"})
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(selected.read_text().strip(), str(node_prefix / "bin" / "node"))

    def test_macos_keeps_correct_path_pins_without_homebrew_override(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            brew_called = fake_bin / "brew-called"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v24.21.0")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            self.fake_command(fake_bin, "pnpm", "if [ \"${1:-}\" = --version ]; then echo 12.8.1; fi")
            self.fake_command(fake_bin, "brew", f"touch '{brew_called}'; exit 1")

            result = self.run_helper("dev", env={"PATH": f"{fake_bin}:{os.environ['PATH']}"})
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(brew_called.exists())

    def test_macos_missing_homebrew_fallback_keeps_strict_pin_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v20.0.0")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.0.0 (test)'")
            self.fake_command(fake_bin, "pnpm", "if [ \"${1:-}\" = --version ]; then echo 12.8.1; fi")
            self.fake_command(fake_bin, "brew", "exit 1")

            result = self.run_helper("dev", env={"PATH": f"{fake_bin}:{os.environ['PATH']}"})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Node 24.21.0 is required", result.stderr)

    def test_macos_build_does_not_add_apple_signing_identity_by_default(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            target = fake_bin / "target"
            pnpm_env = fake_bin / "pnpm-env"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v24.21.0")
            self.fake_command(fake_bin, "pnpm", f"if [ \"${{1:-}}\" = --version ]; then echo 12.8.1; else printf '%s\\n' \"${{APPLE_SIGNING_IDENTITY:-}}\" > '{pnpm_env}'; mkdir -p \"${{CARGO_TARGET_DIR:?}}/release/bundle/macos/Peppy.app\"; fi")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            result = self.run_helper("build", env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "CARGO_TARGET_DIR": str(target)})
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(pnpm_env.read_text().strip(), "")

    def test_macos_build_passes_signing_identity_to_pnpm_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            target = fake_bin / "target"
            pnpm_env = fake_bin / "pnpm-env"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v24.21.0")
            self.fake_command(fake_bin, "pnpm", f"if [ \"${{1:-}}\" = --version ]; then echo 12.8.1; else printf '%s\\n' \"${{APPLE_SIGNING_IDENTITY:-}}\" > '{pnpm_env}'; mkdir -p \"${{CARGO_TARGET_DIR:?}}/release/bundle/macos/Peppy.app\"; fi")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            self.fake_command(fake_bin, "security", "echo '  2) 0123456789ABCDEF0123456789ABCDEF01234567 \"Apple Development: test@example.com (ABCDEF1234)\"'")
            result = self.run_helper(
                "build",
                env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "CARGO_TARGET_DIR": str(target), "PEPPY_MACOS_SIGNING_IDENTITY": "Apple Development: test@example.com (ABCDEF1234)"}
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(pnpm_env.read_text().strip(), "Apple Development: test@example.com (ABCDEF1234)")

    def test_macos_build_rejects_empty_signing_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v24.21.0")
            self.fake_command(fake_bin, "pnpm", "if [ \"${1:-}\" = --version ]; then echo 12.8.1; fi")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            result = self.run_helper(
                "build",
                env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "PEPPY_MACOS_SIGNING_IDENTITY": ""}
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("empty", result.stderr.lower() if result.stderr else "")

    def test_macos_build_rejects_adhoc_signing_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v24.21.0")
            self.fake_command(fake_bin, "pnpm", "if [ \"${1:-}\" = --version ]; then echo 12.8.1; fi")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            result = self.run_helper(
                "build",
                env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "PEPPY_MACOS_SIGNING_IDENTITY": "-"}
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Ad-hoc", result.stderr)

    def test_macos_build_rejects_unknown_signing_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v24.21.0")
            self.fake_command(fake_bin, "pnpm", "if [ \"${1:-}\" = --version ]; then echo 12.8.1; fi")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            self.fake_command(fake_bin, "security", "echo '  2) 0123456789ABCDEF0123456789ABCDEF01234567 \"Apple Development: other@example.com (ABCDEF1234)\"'")
            result = self.run_helper(
                "build",
                env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "PEPPY_MACOS_SIGNING_IDENTITY": "Apple Development: test@example.com (ABCDEF1234)"}
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("not found", result.stderr)

    def test_macos_build_rejects_partial_signing_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            pnpm_called = fake_bin / "pnpm-called"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v24.21.0")
            self.fake_command(fake_bin, "pnpm", f"touch '{pnpm_called}'")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            self.fake_command(fake_bin, "security", "echo '  2) 0123456789ABCDEF0123456789ABCDEF01234567 \"Apple Development: test@example.com (ABCDEF1234)\"'")
            result = self.run_helper(
                "build",
                env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "PEPPY_MACOS_SIGNING_IDENTITY": "Apple Development"}
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(pnpm_called.exists())

    def test_macos_build_verifies_identity_before_running_pnpm(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            pnpm_called = fake_bin / "pnpm-called"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v24.21.0")
            self.fake_command(fake_bin, "pnpm", f"touch '{pnpm_called}'")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            self.fake_command(fake_bin, "security", "echo '  2) 0123456789ABCDEF0123456789ABCDEF01234567 \"Other Identity\"'")
            result = self.run_helper(
                "build",
                env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "PEPPY_MACOS_SIGNING_IDENTITY": "Apple Development: test@example.com (ABCDEF1234)"}
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(pnpm_called.exists())

    def test_macos_build_accepts_sha1_identity_match(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            target = fake_bin / "target"
            pnpm_env = fake_bin / "pnpm-env"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "node", "echo v24.21.0")
            self.fake_command(fake_bin, "pnpm", f"if [ \"${{1:-}}\" = --version ]; then echo 12.8.1; else printf '%s\\n' \"${{APPLE_SIGNING_IDENTITY:-}}\" > '{pnpm_env}'; mkdir -p \"${{CARGO_TARGET_DIR:?}}/release/bundle/macos/Peppy.app\"; fi")
            self.fake_command(fake_bin, "rustc", "echo 'rustc 1.98.1 (test)'")
            self.fake_command(fake_bin, "security", "echo '  2) ABCDEF1234567890ABCDEF1234567890ABCDEF12 \"Apple Development: test@example.com\"'")
            result = self.run_helper(
                "build",
                env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "CARGO_TARGET_DIR": str(target), "PEPPY_MACOS_SIGNING_IDENTITY": "ABCDEF1234567890ABCDEF1234567890ABCDEF12"}
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(pnpm_env.read_text().strip(), "ABCDEF1234567890ABCDEF1234567890ABCDEF12")

    def test_macos_open_ignores_signing_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            fake_bin = pathlib.Path(temporary)
            target = pathlib.Path(temporary) / "target"
            bundle = target / "release/bundle/macos/Peppy.app"
            bundle.mkdir(parents=True)
            arguments = pathlib.Path(temporary) / "open-arguments"
            self.fake_command(fake_bin, "uname", "echo Darwin")
            self.fake_command(fake_bin, "open", f"printf '%s\\n' \"$@\" > '{arguments}'")
            result = self.run_helper(
                "open",
                env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "CARGO_TARGET_DIR": str(target), "PEPPY_MACOS_SIGNING_IDENTITY": "Apple Development: test@example.com (ABCDEF1234)"}
            )
            self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
