import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / "infra/dev/ios.sh"
RUNTIME = "com.apple.CoreSimulator.SimRuntime.iOS-26-0"


def phone(udid="A", name="iPhone 17", state="Shutdown", available=True):
    return {"udid": udid, "name": name, "state": state, "isAvailable": available,
            "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17"}


class IOSHelperTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.tools = self.root / "bin"
        self.tools.mkdir()
        # /usr/bin/python3 on macOS delegates to xcrun; don't route that shim
        # through our fake SDK tool when the helper executes its real selector.
        (self.tools / "python3").symlink_to(sys.executable)
        self.log = self.root / "calls"
        self.repo = self.root / "repo"
        generated = self.repo / "apps/ios/Generated"
        generated.mkdir(parents=True)
        (generated / "bindings.swift").write_text("current bindings\n")
        self.developer = self.root / "Xcode Beta.app/Contents/Developer"
        (self.developer / "Applications/Simulator.app").mkdir(parents=True)
        (self.developer / "usr/bin").mkdir(parents=True)
        # Setup for DeviceHub test: Xcode27 style with ../Applications/DeviceHub.app relative path
        self.xcode27_developer = self.root / "Xcode27.app/Contents/Developer"
        self.xcode27_devicehub = self.root / "Xcode27.app/Contents/Applications/DeviceHub.app"
        self.xcode27_devicehub.mkdir(parents=True)
        (self.xcode27_developer / "usr/bin").mkdir(parents=True)
        self.sdk = self.root / "sdk"
        self.sdk.mkdir()
        self.env = os.environ.copy()
        for name in ("PEPPY_IOS_SIMULATOR", "CARGO_TARGET_DIR", "SDKROOT"):
            self.env.pop(name, None)
        self.env.update({
            "PATH": f"{self.tools}:{os.environ['PATH']}",
            "DEVELOPER_DIR": str(self.developer),
            "PEPPY_REPOSITORY_ROOT": str(self.repo),
            "PEPPY_IOS_ARTIFACTS": str(self.root / "derived"),
            "PEPPY_IOS_SCRATCH": str(self.root / "scratch"),
            "IOS_LOG": str(self.log), "IOS_ROOT": str(self.repo),
            "IOS_SDK": str(self.sdk), "IOS_TOOL": str(self.tools / "tool"),
            "IOS_DEVICES": str(self.root / "devices.json"),
            "IOS_RUNTIMES": str(self.root / "runtimes.json"),
        })
        self.inventory({RUNTIME: [phone()]})
        self.fake("uname", 'if [ "$1" = -s ]; then echo "${IOS_OS:-Darwin}"; else echo "${IOS_ARCH:-arm64}"; fi\n')
        self.fake("xcode-select", f"echo '{self.developer}'\n")
        self.fake("xcodebuild", '''
echo "xcodebuild $*" >> "$IOS_LOG"
echo "developer=$DEVELOPER_DIR" >> "$IOS_LOG"
if [ "$1" = -version ]; then echo "Xcode ${IOS_XCODE_VERSION:-26.0}"; exit 0; fi
[ "${IOS_FAIL:-}" != xcode ] || exit 1
if [ "${IOS_FAIL:-}" != product ]; then
  mkdir -p "$PEPPY_IOS_ARTIFACTS/Build/Products/Debug-iphonesimulator/PeppyMobile.app"
fi
''')
        self.fake("xcrun", '''
echo "xcrun $*" >> "$IOS_LOG"
case "$*" in
  *--show-sdk-path*) echo "$IOS_SDK" ;;
  *'--find clang'*|*'--find ar'*|*'--find ranlib'*) echo "$IOS_TOOL" ;;
  *'list devices available -j'*) cat "$IOS_DEVICES" ;;
  *'list runtimes -j'*) cat "$IOS_RUNTIMES" ;;
  *'bootstatus '*) [ "${IOS_FAIL:-}" != boot ] || exit 1 ;;
  *'install '*) [ "${IOS_FAIL:-}" != install ] || exit 1 ;;
  *'launch '*) [ "${IOS_FAIL:-}" != launch ] || exit 1 ;;
esac
exit 0
''')
        self.fake("rustup", 'echo "rustup $*" >> "$IOS_LOG"\n')
        self.fake("cargo", '''
echo "cargo $*" >> "$IOS_LOG"
echo "cwd=$PWD" >> "$IOS_LOG"
case "$*" in
  *'--target '*)
    [ "${IOS_FAIL:-}" != rust ] || exit 1
    echo "compiler=${CC_aarch64_apple_ios_sim:-${CC_x86_64_apple_ios:-}} sdk=$SDKROOT flags=$CFLAGS" >> "$IOS_LOG"
    ;;
  *' generate '*)
    out=; previous=
    for arg in "$@"; do
      if [ "$previous" = --out-dir ]; then out="$arg"; fi
      previous="$arg"
    done
    cp -R "$IOS_ROOT/apps/ios/Generated/." "$out"
    if [ "${IOS_FAIL:-}" = bindings ]; then echo stale >> "$out/bindings.swift"; fi
    ;;
esac
''')
        self.fake("open", 'echo "open $*" >> "$IOS_LOG"\n')
        self.fake("bash", 'echo "backend $*" >> "$IOS_LOG"\n[ "${IOS_FAIL:-}" != backend ] || exit 1\n')
        self.fake("tool", "exit 0\n")
        for name in ("xcodebuild", "xcrun"):
            (self.developer / "usr/bin" / name).symlink_to(self.tools / name)
            (self.xcode27_developer / "usr/bin" / name).symlink_to(self.tools / name)

    def fake(self, name, body):
        path = self.tools / name
        path.write_text("#!/bin/sh\nset -eu\n" + body)
        path.chmod(0o755)

    def inventory(self, devices, runtimes=None):
        if runtimes is None:
            runtimes = [{"identifier": identifier, "isAvailable": True} for identifier in devices]
        (self.root / "devices.json").write_text(json.dumps({"devices": devices}))
        (self.root / "runtimes.json").write_text(json.dumps({"runtimes": runtimes}))

    def run_helper(self, command="check", **values):
        return subprocess.run(["/bin/bash", str(HELPER), command], cwd=self.root,
                              env=self.env | values, text=True, capture_output=True, timeout=20)

    def calls(self):
        return self.log.read_text() if self.log.exists() else ""

    def test_check_selects_explicit_udid_without_mutating_simulator(self):
        result = self.run_helper(PEPPY_IOS_SIMULATOR="A")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"developer={self.developer}", self.calls())
        self.assertIn("A", result.stdout)
        for effect in ("backend", "bootstatus", "install", "launch", "cargo", "open"):
            self.assertNotIn(effect, self.calls())

    def test_selected_full_xcode_with_nonstandard_name_is_preserved(self):
        self.env.pop("DEVELOPER_DIR")
        result = self.run_helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"developer={self.developer}", self.calls())

    def test_explicit_invalid_xcode_does_not_fall_back(self):
        result = self.run_helper(DEVELOPER_DIR=str(self.root / "missing"))
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("backend", self.calls())

    def test_rejects_non_macos_and_old_xcode_before_backend(self):
        for values in ({"IOS_OS": "Linux"}, {"IOS_XCODE_VERSION": "25.0"}):
            with self.subTest(values=values):
                result = self.run_helper("run", **values)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("backend", self.calls())

    def test_rejects_unavailable_and_old_runtimes_before_backend(self):
        for runtimes in ([{"identifier": RUNTIME, "isAvailable": False}],
                         [{"identifier": "com.apple.CoreSimulator.SimRuntime.iOS-25-0", "isAvailable": True}]):
            with self.subTest(runtimes=runtimes):
                self.inventory({RUNTIME: [phone()]}, runtimes)
                result = self.run_helper("run")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Components", result.stderr)
                self.assertNotIn("backend", self.calls())

    def test_renamed_iphone_can_be_selected_by_exact_name(self):
        self.inventory({RUNTIME: [phone(name="My test phone")]})
        result = self.run_helper(PEPPY_IOS_SIMULATOR="My test phone")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("A", result.stdout)

    def test_rejects_ambiguous_names_and_multiple_booted_iphones(self):
        for override, state, error in (("Same", "Shutdown", "ambiguous"), ("", "Booted", "multiple booted")):
            with self.subTest(override=override):
                self.inventory({RUNTIME: [phone("A", "Same", state), phone("B", "Same", state)]})
                result = self.run_helper(PEPPY_IOS_SIMULATOR=override)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(error, result.stderr)

    def test_rejects_unavailable_device_and_ipad_overrides(self):
        ipad = phone("B", "iPad Pro")
        ipad["deviceTypeIdentifier"] = "com.apple.CoreSimulator.SimDeviceType.iPad-Pro"
        self.inventory({RUNTIME: [phone(available=False), ipad]})
        for override in ("A", "B", "missing"):
            with self.subTest(override=override):
                result = self.run_helper(PEPPY_IOS_SIMULATOR=override)
                self.assertNotEqual(result.returncode, 0)

    def test_default_reuses_one_booted_iphone(self):
        self.inventory({RUNTIME: [phone("A"), phone("B", state="Booted")]})
        result = self.run_helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("B", result.stdout)

    def test_default_selects_newest_runtime_numerically_and_deterministically(self):
        old = "com.apple.CoreSimulator.SimRuntime.iOS-26-9"
        new = "com.apple.CoreSimulator.SimRuntime.iOS-26-10"
        self.inventory({old: [phone("OLD")], new: [phone("NEW-B", "iPhone B"), phone("NEW-A", "iPhone A")]})
        result = self.run_helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NEW-A", result.stdout)

    def test_custom_cargo_target_directory_is_consistent_across_build_and_link(self):
        target = self.root / "rust outputs"
        result = self.run_helper("run", CARGO_TARGET_DIR=str(target))
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertIn(f"--target-dir {target}", calls)
        self.assertIn(f"--library {target}/debug/libpeppy_mobile_bindings.dylib", calls)
        self.assertIn(f'LIBRARY_SEARCH_PATHS="{target}/aarch64-apple-ios-sim/debug"', calls)

    def test_intel_mac_builds_x86_64_simulator_library(self):
        result = self.run_helper("run", IOS_ARCH="x86_64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--target x86_64-apple-ios", self.calls())
        self.assertIn("-target x86_64-apple-ios26.0-simulator", self.calls())
        self.assertIn("ARCHS=x86_64", self.calls())

    def test_run_builds_current_debug_sources_before_install_and_launch(self):
        result = self.run_helper("run")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertIn(f"cwd={self.repo}", calls)
        self.assertIn("--target aarch64-apple-ios-sim", calls)
        self.assertIn("-configuration Debug -sdk iphonesimulator", calls)
        self.assertIn("-mios-simulator-version-min=26.0", calls)
        self.assertNotIn("--release", calls)
        expected = ["backend", "bootstatus A -b", "open ", "cargo build", "-configuration Debug", "install A", "launch --terminate-running-process A dev.peppy.mobile"]
        positions = [calls.index(value) for value in expected]
        self.assertEqual(positions, sorted(positions))
        self.assertIn(f"open {self.developer}/Applications/Simulator.app --args -CurrentDeviceUDID A", calls)
        self.assertEqual(list((self.root / "scratch").iterdir()), [])

    def test_run_enables_local_simulator_signing(self):
        result = self.run_helper("run")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertIn("CODE_SIGNING_ALLOWED=YES", calls)
        self.assertNotIn("CODE_SIGNING_ALLOWED=NO", calls)

    def test_run_reuses_booted_device_without_a_second_boot(self):
        self.inventory({RUNTIME: [phone(state="Booted")]})
        result = self.run_helper("run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("simctl boot A", self.calls())
        self.assertIn("bootstatus A -b", self.calls())

    def test_build_failures_and_binding_drift_never_deploy_stale_app(self):
        for failure in ("rust", "xcode", "bindings", "product"):
            with self.subTest(failure=failure):
                self.log.unlink(missing_ok=True)
                shutil.rmtree(self.root / "derived", ignore_errors=True)
                result = self.run_helper("run", IOS_FAIL=failure)
                self.assertNotEqual(result.returncode, 0)
                calls = self.calls()
                self.assertIn("backend", calls)
                self.assertIn("cargo build", calls)
                self.assertNotIn("install A", calls)
                self.assertNotIn("launch --terminate-running-process", calls)
                self.assertNotIn("unbound variable", result.stderr)

    def test_install_failure_never_launches(self):
        result = self.run_helper("run", IOS_FAIL="install")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("install A", self.calls())
        self.assertNotIn("launch --terminate-running-process", self.calls())

    def test_backend_or_boot_failure_never_builds(self):
        for failure in ("backend", "boot"):
            with self.subTest(failure=failure):
                self.log.unlink(missing_ok=True)
                result = self.run_helper("run", IOS_FAIL=failure)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("backend", self.calls())
                self.assertNotIn("cargo build", self.calls())

    def test_devicehub_only_xcode27_passes_check(self):
        """Xcode27 with only DeviceHub.app at ../ should pass full_xcode check."""
        env = self.env.copy()
        env["DEVELOPER_DIR"] = str(self.xcode27_developer)
        env["PEPPY_IOS_SIMULATOR"] = "A"
        result = subprocess.run(["/bin/bash", str(HELPER), "check"], cwd=self.root,
                                env=env, text=True, capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("A", result.stdout)

    def test_devicehub_run_opens_selected_xcode27_frontend_without_legacy_args(self):
        env = self.env.copy()
        env["DEVELOPER_DIR"] = str(self.xcode27_developer)
        result = subprocess.run(["/bin/bash", str(HELPER), "run"], cwd=self.root,
                                env=env, text=True, capture_output=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertIn(f"developer={self.xcode27_developer}", calls)
        self.assertIn(f"open {self.xcode27_developer}/../Applications/DeviceHub.app", calls)
        self.assertNotIn("--args -CurrentDeviceUDID", calls)


if __name__ == "__main__":
    unittest.main()
