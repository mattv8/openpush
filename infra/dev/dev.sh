#!/usr/bin/env bash
set -euo pipefail

ROOT="${OPENPUSH_REPOSITORY_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
cd "$ROOT"
recipe="${1:-}"
shift || true

require_actions_installer() {
  command -v python3 >/dev/null || { echo "dev-setup requires python3" >&2; exit 1; }
  if [[ ! -f infra/dev/install-actions.py ]]; then
    echo "dev-setup needs infra/dev/install-actions.py (provided by the actions workflow lane); rerun after it is available" >&2
    exit 1
  fi
}

require_container_tooling() {
  command -v docker >/dev/null || { echo "container development requires docker" >&2; exit 1; }
  docker compose version >/dev/null 2>&1 || { echo "container development requires docker compose" >&2; exit 1; }
}

case "$recipe" in
  dev-setup)
    require_actions_installer
    if [[ -L .env ]]; then
      echo "refusing to replace symlinked .env" >&2
      exit 1
    fi
    if [[ ! -e .env ]]; then
      python3 - <<'PY'
import os
import secrets

values = {
    "OPENPUSH_ENV": "development",
    "POSTGRES_DB": "openpush",
    "POSTGRES_USER": "openpush",
    "POSTGRES_PASSWORD": "synthetic-" + secrets.token_urlsafe(32),
    "S3_ACCESS_KEY": "synthetic-" + secrets.token_urlsafe(16),
    "S3_SECRET_KEY": "synthetic-" + secrets.token_urlsafe(32),
    "S3_BUCKET": "openpush-private",
    "OPENPUSH_REPLAY_RETENTION_DAYS": "30",
    "VAULT_ATTACHMENT_QUOTA_BYTES": "536870912",
    "PUBLIC_API_URL": "http://127.0.0.1:8080",
    "PUBLIC_ATTACHMENT_URL": "http://127.0.0.1:8080",
}
fd = os.open(".env", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as output:
    for key, value in values.items():
        output.write(f"{key}={value}\n")
PY
      echo "Created .env with synthetic local credentials (mode 0600)." >&2
    fi
    exec python3 infra/dev/install-actions.py "$@" ;;
  dev-demo)
    require_container_tooling
    exec python3 infra/dev/demo.py "$@" ;;
  dev-up|dev-down|dev-build|dev-test)
    require_container_tooling
    exec just "$recipe" "$@" ;;
  android-build|android-emulator|android-deploy|android-smoke|android-sms|desktop-dev|desktop-bundle|desktop-open)
    exec just "$recipe" "$@" ;;
  *)
    echo "unknown development recipe: ${recipe:-<missing>}" >&2
    echo "run 'just --list' for available recipes" >&2
    exit 64 ;;
esac
