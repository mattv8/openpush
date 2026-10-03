import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "infra/audit/check-vendored.py"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
CHECKSUM = "a" * 64


def load_checker():
    spec = importlib.util.spec_from_file_location("check_vendored", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class VendoredAuditTests(unittest.TestCase):
    def setUp(self):
        self.checker = load_checker()
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)

    def tearDown(self):
        self.temp_dir.cleanup()

    def add_package(self, directory="crate", name="crate", version="1.2.3", lock=None):
        crate = self.root / "vendor" / directory
        crate.mkdir(parents=True)
        (crate / "Cargo.toml").write_text(
            f"[package]\nname = '{name}' # valid TOML\nversion = '{version}'\n"
        )
        if lock is not None:
            (crate / "upstream-audit.lock").write_text(lock)
        return crate

    def lock(self, name="crate", version="1.2.3", source=REGISTRY, checksum=CHECKSUM):
        return (
            "version = 3\n\n[[package]]\n"
            f"name = '{name}'\nversion = '{version}'\nsource = '{source}'\n"
            f"checksum = '{checksum}'\n"
        )

    def run_main(self):
        with patch.object(self.checker, "get_repo_root", return_value=self.root):
            return self.checker.main()

    def test_valid_toml_with_single_quotes_and_comments_is_audited(self):
        self.add_package(lock=self.lock())
        with patch.object(self.checker, "run_cargo_audit", return_value=True) as audit:
            self.assertEqual(self.run_main(), 0)
        audit.assert_called_once()

    def test_commented_matching_lock_version_does_not_hide_actual_drift(self):
        lock = self.lock(version="2.0.0").replace(
            "version = '2.0.0'", "# version = '1.2.3'\nversion = '2.0.0'"
        )
        self.add_package(lock=lock)
        with patch.object(self.checker, "run_cargo_audit") as audit:
            self.assertEqual(self.run_main(), 1)
        audit.assert_not_called()

    def test_non_crates_io_registry_is_rejected(self):
        self.add_package(lock=self.lock(source="registry+https://example.test/index"))
        with patch.object(self.checker, "run_cargo_audit") as audit:
            self.assertEqual(self.run_main(), 1)
        audit.assert_not_called()

    def test_missing_vendor_directory_fails_closed(self):
        self.assertEqual(self.run_main(), 1)

    def test_vendor_package_requires_manifest(self):
        (self.root / "vendor" / "crate").mkdir(parents=True)
        self.assertEqual(self.run_main(), 1)

    def test_vendor_package_requires_lock(self):
        self.add_package()
        self.assertEqual(self.run_main(), 1)

    def test_lock_requires_crates_io_checksum(self):
        self.add_package(lock=self.lock(checksum="not-a-checksum"))
        self.assertEqual(self.run_main(), 1)

    def test_all_entries_validate_before_any_audit_runs(self):
        self.add_package("a-valid", lock=self.lock())
        self.add_package("z-invalid", version="1.2.4", lock=self.lock())
        with patch.object(self.checker, "run_cargo_audit", return_value=True) as audit:
            self.assertEqual(self.run_main(), 1)
        audit.assert_not_called()

    def test_audit_failure_and_argument_contract_propagate(self):
        self.add_package(lock=self.lock())
        with patch.object(self.checker.subprocess, "run") as run:
            run.return_value.returncode = 1
            run.return_value.stdout = ""
            run.return_value.stderr = "audit failed\n"
            self.assertEqual(self.run_main(), 1)
        self.assertEqual(run.call_args.args[0][:5], ["cargo", "audit", "--deny", "warnings", "--file"])


if __name__ == "__main__":
    unittest.main()
