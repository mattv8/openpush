#!/usr/bin/env python3
"""Run the disposable, simulated gateway exercise without touching dev data."""
import argparse
import json
import os
import selectors
import secrets
import signal
import socket
import stat
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Optional
from urllib.parse import urlsplit
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[2]
RUNTIME_TIMEOUT = 60
COLD_BUILD_TIMEOUT = 600


def private_file(path: Path, content: str) -> Path:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as output:
        output.write(content)
    return path


def make_run_directory(base: Path) -> Path:
    base.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(base, 0o700)
    run = base / f"gateway-demo-{uuid.uuid4().hex}"
    run.mkdir(mode=0o700)
    return run


def demo_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def synthetic_values(port: int):
    token = uuid.uuid4().hex
    values = {
        "PEPPY_ENV": "development",
        "POSTGRES_DB": "peppy_demo",
        "POSTGRES_USER": "peppy_demo",
        "POSTGRES_PASSWORD": f"synthetic-{token}",
        "S3_ACCESS_KEY": f"synthetic-{token[:16]}",
        "S3_SECRET_KEY": f"synthetic-{uuid.uuid4().hex}",
        "S3_BUCKET": "peppy-private",
        "PUBLIC_API_URL": f"http://127.0.0.1:{port}",
        "PUBLIC_ATTACHMENT_URL": f"http://127.0.0.1:{port}",
        "BIND_ADDR": f"127.0.0.1:{port}",
        "PEPPY_ISOLATED_DEMO": "1",
    }
    return values


def write_environment(run: Path, port: int) -> Path:
    values = synthetic_values(port)
    return private_file(run / "demo.env", "".join(f"{key}={value}\n" for key, value in values.items()))


def sanitized_environment(environment, values):
    return {key: value for key, value in environment.items() if key not in values and not key.startswith("COMPOSE_")}


def timeout_from_environment(name, default):
    value = os.environ.get(name, str(default))
    try:
        timeout = int(value)
    except ValueError:
        raise SystemExit(f"{name} must be a whole number of seconds")
    if timeout <= 0:
        raise SystemExit(f"{name} must be greater than zero")
    return timeout


def credential_metadata(path: Path):
    metadata = {
        "path": "gateway/credentials.json",
        "exists": False,
        "type": "missing",
        "mode": None,
        "uid": None,
        "gid": None,
        "bytes": None,
        "json_valid": False,
        "version": None,
        "version_valid": False,
        "nonempty_vault_id": False,
        "nonempty_device_id": False,
        "nonempty_token": False,
        "canonical_origin": False,
    }
    try:
        info = path.stat()
    except OSError:
        return metadata, False
    metadata.update({"exists": True, "mode": format(stat.S_IMODE(info.st_mode), "04o"), "uid": info.st_uid, "gid": info.st_gid, "bytes": info.st_size})
    if stat.S_ISREG(info.st_mode):
        metadata["type"] = "regular"
    elif stat.S_ISDIR(info.st_mode):
        metadata["type"] = "directory"
    else:
        metadata["type"] = "other"
    try:
        state = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return metadata, False
    if not isinstance(state, dict):
        return metadata, False
    metadata["json_valid"] = True
    version = state.get("version")
    metadata["version"] = version if isinstance(version, int) else None
    metadata["version_valid"] = version == 1
    metadata["nonempty_vault_id"] = isinstance(state.get("vaultId"), str) and bool(state["vaultId"])
    metadata["nonempty_device_id"] = isinstance(state.get("deviceId"), str) and bool(state["deviceId"])
    metadata["nonempty_token"] = isinstance(state.get("deviceToken"), str) and bool(state["deviceToken"])
    origin = state.get("origin")
    if isinstance(origin, str):
        try:
            parsed = urlsplit(origin)
            metadata["canonical_origin"] = parsed.scheme == "http" and parsed.hostname in {"127.0.0.1", "localhost"} and parsed.username is None and parsed.password is None and parsed.port is not None and parsed.path in {"", "/"} and not parsed.query and not parsed.fragment
        except ValueError:
            pass
    valid = metadata["type"] == "regular" and metadata["mode"] == "0600" and metadata["uid"] == os.getuid() and metadata["gid"] == os.getgid() and metadata["bytes"] > 0 and metadata["json_valid"] and metadata["version_valid"] and metadata["nonempty_vault_id"] and metadata["nonempty_device_id"] and metadata["nonempty_token"] and metadata["canonical_origin"]
    return metadata, valid


class ChildController:
    def __init__(self):
        self.children = []

    def add(self, child: subprocess.Popen) -> subprocess.Popen:
        self.children.append(child)
        return child

    def cleanup(self):
        for child in self.children:
            if child.poll() is None:
                child.terminate()
        for child in self.children:
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)


class ProcessOutput:
    """Binary pipe reader: partial lines and EOF never block the controller."""
    def __init__(self, process: subprocess.Popen, log_path: Path):
        self.process = process
        self.log = open(log_path, "ab", buffering=0)
        self.selector = selectors.DefaultSelector()
        self.buffers = {}
        for stream, name in ((process.stdout, "stdout"), (process.stderr, "stderr")):
            if stream is not None:
                os.set_blocking(stream.fileno(), False)
                self.selector.register(stream, selectors.EVENT_READ, name)
                self.buffers[name] = b""

    def _read(self, timeout: float):
        for key, _ in self.selector.select(timeout):
            data = os.read(key.fileobj.fileno(), 65536)
            if not data:
                self.selector.unregister(key.fileobj)
                continue
            self.log.write(key.data.encode() + b": " + data)
            self.buffers[key.data] += data

    def checkpoint(self, source: str) -> int:
        return len(self.buffers.get(source, b""))

    def wait_for(self, marker: str, source: str, timeout: float, after=0) -> bool:
        needle = marker.encode()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if needle in self.buffers.get(source, b"")[after:]:
                return True
            self._read(max(0, deadline - time.monotonic()))
            if self.process.poll() is not None and not self.selector.get_map():
                break
        return needle in self.buffers.get(source, b"")[after:]

    def wait_for_exit(self, timeout: float) -> Optional[int]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self._read(min(0.2, max(0, deadline - time.monotonic())))
            if self.process.poll() is not None:
                while self.selector.get_map():
                    self._read(0)
                return self.process.returncode
        return None

    def close(self):
        self.selector.close()
        self.log.close()
        for stream in (self.process.stdout, self.process.stderr):
            if stream is not None:
                stream.close()


def checked(command, *, env, timeout=RUNTIME_TIMEOUT, **kwargs):
    return subprocess.run(command, env=env, timeout=timeout, check=True, **kwargs)


def run_inside_container() -> None:
    if os.environ.get("PEPPY_ISOLATED_DEMO") != "1":
        raise SystemExit("demo requires isolated dev-demo context")
    run = Path(os.environ.get("PEPPY_DEMO_RUN", "/artifacts/gateway-demo"))
    run.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(run, 0o700)
    gateway, desktop = run / "gateway", run / "desktop"
    gateway.mkdir(mode=0o700, exist_ok=True)
    desktop.mkdir(mode=0o700, exist_ok=True)
    env = os.environ.copy()
    server_env = {key: value for key, value in env.items() if key != "PEPPY_SIMULATOR_PASSPHRASE"}
    controller = ChildController()
    target_dir = Path(env.get("CARGO_TARGET_DIR", "target")) / "debug"
    server_binary = str(target_dir / "peppy-server")
    simulator_binary = str(target_dir / "peppy-gateway-simulator")
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt
    old_handlers = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        cold_build_timeout = timeout_from_environment("PEPPY_DEMO_CARGO_TIMEOUT", COLD_BUILD_TIMEOUT)
        checked(["cargo", "build", "--locked", "--quiet", "-p", "peppy-server", "-p", "peppy-gateway-simulator"], env=env, timeout=cold_build_timeout)
        checked([server_binary, "migrate"], env=server_env)
        server_log = os.fdopen(os.open(run / "server.log", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb")
        server = controller.add(subprocess.Popen([server_binary], env=server_env, stdout=server_log, stderr=subprocess.STDOUT))
        deadline = time.monotonic() + RUNTIME_TIMEOUT
        while time.monotonic() < deadline:
            try:
                with urlopen(env["PUBLIC_API_URL"] + "/readyz", timeout=2):
                    break
            except OSError:
                time.sleep(0.2)
        else:
            raise RuntimeError("demo server did not become ready")
        checked([simulator_binary, "prepare-vault", str(gateway)], env=env)
        private_file(desktop / "vault-bootstrap.json", (gateway / "vault-bootstrap.json").read_text())
        bootstrap = json.loads((gateway / "vault-bootstrap.json").read_text())
        owner_env = server_env | {
            "PEPPY_OWNER_PUBLIC_KEY_PROFILE": json.dumps(bootstrap["profile"], separators=(",", ":")),
            "PEPPY_OWNER_VAULT_CHECK_HEADER_HEX": json.dumps(bootstrap["header"], separators=(",", ":")).encode().hex(),
            "PEPPY_OWNER_PROFILE_FINGERPRINT": bootstrap["fingerprint"],
            "PEPPY_OWNER_KEY_EPOCH": str(bootstrap["profile"]["key_epoch"]),
        }
        owner = checked([server_binary, "create-owner"], env=owner_env, stdout=subprocess.PIPE, text=True)
        fields = dict(line.split("=", 1) for line in owner.stdout.splitlines())
        owner_path = private_file(run / "owner.json", json.dumps({"version": 1, "origin": env["PUBLIC_API_URL"], "vaultId": fields["vault_id"], "deviceId": fields["device_id"], "deviceToken": fields["device_token"]}) + "\n")
        checked([simulator_binary, "pair", str(gateway), str(owner_path), "gateway"], env=env)
        checked([simulator_binary, "pair", str(desktop), str(owner_path), "device"], env=env)
        credentials_metadata, credentials_valid = credential_metadata(gateway / "credentials.json")
        private_file(run / "credential-metadata.json", json.dumps(credentials_metadata, sort_keys=True) + "\n")
        if not credentials_valid:
            raise RuntimeError("gateway credential metadata validation failed; inspect private credential-metadata.json")
        credential = json.loads((desktop / "desktop-import.json").read_text())
        def replay():
            request = Request(credential["origin"] + "/v1/events?after=0&limit=200", headers={"Authorization": "Bearer " + credential["deviceToken"]})
            with urlopen(request, timeout=10) as response:
                return json.load(response)["events"]
        simulator = controller.add(subprocess.Popen([simulator_binary, "run", str(gateway)], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE))
        output = ProcessOutput(simulator, run / "simulator.log")
        try:
            if not output.wait_for("gateway running", "stdout", RUNTIME_TIMEOUT):
                raise RuntimeError("simulator did not start")
            if not output.wait_for("phase=media", "stderr", RUNTIME_TIMEOUT):
                raise RuntimeError("simulator did not reach media phase")
            baseline_events = len(replay())
            sync_checkpoint = output.checkpoint("stderr")
            simulator.stdin.write(b'{"type":"incoming","senderAddress":"+15555550123","body":"synthetic runnable demo message","providerMessageId":"demo-incoming-1"}\n')
            simulator.stdin.flush()
            if not output.wait_for("phase=sync", "stderr", RUNTIME_TIMEOUT, after=sync_checkpoint):
                raise RuntimeError("simulator did not sync")
            media_checkpoint = output.checkpoint("stderr")
            if not output.wait_for("phase=media", "stderr", RUNTIME_TIMEOUT, after=media_checkpoint):
                raise RuntimeError("simulator did not return to media phase")
            final_sync_checkpoint = output.checkpoint("stderr")
            if not output.wait_for("phase=sync", "stderr", RUNTIME_TIMEOUT, after=final_sync_checkpoint):
                raise RuntimeError("simulator did not complete post-media sync")
            simulator.stdin.write(b'{"type":"quit"}\n')
            simulator.stdin.flush()
            exit_code = output.wait_for_exit(RUNTIME_TIMEOUT)
            if exit_code != 0:
                raise RuntimeError(f"simulator exited {exit_code}; inspect private log {run / 'simulator.log'}")
        finally:
            output.close()
        if len(replay()) <= baseline_events:
            raise RuntimeError("real API replay did not contain the new synthetic simulator event")
        database = gateway / "gateway.sqlcipher"
        if not database.is_file() or database.stat().st_size == 0:
            raise RuntimeError("client SQLCipher store was not persisted")
        checked([simulator_binary, "run", str(gateway)], env=env, input=b'{"type":"quit"}\n', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        print("simulated gateway demo verified; no carrier or production behavior was exercised")
    finally:
        controller.cleanup()
        if 'server_log' in locals():
            server_log.close()
        for sig, handler in old_handlers.items():
            signal.signal(sig, handler)


def run_host_demo() -> None:
    os.umask(0o077)
    run = make_run_directory(ROOT / ".opencode" / "dev" / "artifacts")
    port = demo_port()
    values = synthetic_values(port)
    env_file = private_file(run / "demo.env", "".join(f"{key}={value}\n" for key, value in values.items()))
    project = "peppy-demo-" + uuid.uuid4().hex[:12]
    compose = ["docker", "compose", "--project-name", project, "--env-file", str(env_file), "-f", "docker-compose.yml", "-f", "docker/compose.dev.yml"]
    environment = sanitized_environment(os.environ, values)
    environment.update({"DEV_UID": str(os.getuid()), "DEV_GID": str(os.getgid()), "PEPPY_DEMO_RUN": "/artifacts/" + run.name, "PEPPY_SIMULATOR_PASSPHRASE": secrets.token_urlsafe(32)})
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt
    old_handlers = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        for cache in ("cargo", "pnpm", "target"):
            checked(["docker", "volume", "create", f"peppy-dev-{cache}-{os.getuid()}-{os.getgid()}"], env=environment, timeout=RUNTIME_TIMEOUT, stdout=subprocess.DEVNULL, cwd=ROOT)
        build_timeout = timeout_from_environment("PEPPY_DEMO_BUILD_TIMEOUT", COLD_BUILD_TIMEOUT)
        cargo_timeout = timeout_from_environment("PEPPY_DEMO_CARGO_TIMEOUT", COLD_BUILD_TIMEOUT)
        checked(compose + ["build", "dev"], env=environment, timeout=build_timeout, cwd=ROOT)
        checked(compose + ["up", "--detach", "--wait", "postgres", "seaweedfs"], env=environment, timeout=2 * RUNTIME_TIMEOUT, cwd=ROOT)
        checked(compose + ["run", "--rm", "-e", "PEPPY_ISOLATED_DEMO=1", "-e", "PEPPY_DEMO_RUN=/artifacts/" + run.name, "-e", f"PEPPY_DEMO_CARGO_TIMEOUT={cargo_timeout}", "dev", "run", "demo"], env=environment, timeout=cargo_timeout + 8 * RUNTIME_TIMEOUT, cwd=ROOT)
    finally:
        subprocess.run(compose + ["down", "--volumes", "--remove-orphans"], env=environment, timeout=RUNTIME_TIMEOUT, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=ROOT)
        for sig, handler in old_handlers.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--inside-container", action="store_true")
    arguments = parser.parse_args()
    if arguments.inside_container:
        run_inside_container()
    else:
        run_host_demo()
