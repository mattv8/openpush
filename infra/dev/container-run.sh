#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: run build [server|web] | test [rust|web] | demo" >&2
  exit 64
}

sync_source() {
  local source_root=${OPENPUSH_SOURCE_ROOT:-/source}
  local workspace=${OPENPUSH_WORKSPACE:-/workspace}
  test -d "$source_root" || { echo "$source_root read-only mount is required" >&2; exit 73; }
  test -f "$source_root/infra/dev/source-excludes.txt" || { echo "source exclusion list is required" >&2; exit 73; }
  mkdir -p "$workspace/target"
  for path in "$workspace" "$workspace/target" "$CARGO_HOME" "$PNPM_HOME" "$COREPACK_HOME"; do
    if [[ ! -w "$path" ]]; then
      echo "development cache $path is not writable by uid $(id -u); remove only the matching openpush-dev cache volumes or rebuild with your current DEV_UID/DEV_GID" >&2
      exit 73
    fi
  done
  find "$workspace" -mindepth 1 -maxdepth 1 ! -name target ! -name node_modules -exec rm -rf {} +
  (cd "$source_root" && tar -X infra/dev/source-excludes.txt -cf - .) | (cd "$workspace" && tar -xf -)
  if [[ -f "$source_root/.env.example" ]]; then cp "$source_root/.env.example" "$workspace/.env.example"; fi
}

run_with_workspace_lock() {
  local workspace=${OPENPUSH_WORKSPACE:-/workspace} lock_timeout=${OPENPUSH_WORKSPACE_LOCK_TIMEOUT:-600}
  if [[ ! "$lock_timeout" =~ ^[1-9][0-9]*$ ]] || (( lock_timeout > 86400 )); then
    echo "OPENPUSH_WORKSPACE_LOCK_TIMEOUT must be a positive integer no greater than 86400 seconds" >&2
    exit 64
  fi
  mkdir -p "$workspace/target"
  for path in "$workspace" "$workspace/target" "$CARGO_HOME" "$PNPM_HOME" "$COREPACK_HOME"; do
    if [[ ! -w "$path" ]]; then
      echo "development cache $path is not writable by uid $(id -u); remove only the matching openpush-dev cache volumes or rebuild with your current DEV_UID/DEV_GID" >&2
      exit 73
    fi
  done
  exec 9>"$workspace/target/.openpush-run.lock"
  if ! flock -w "$lock_timeout" 9; then
    echo "timed out after ${lock_timeout}s waiting for the persistent development workspace; wait for the existing run to finish" >&2
    exit 75
  fi
  OPENPUSH_WORKSPACE="$workspace" sync_source
  cd "$workspace"
}

case "${1:-}" in
  demo)
    if [[ "${OPENPUSH_ISOLATED_DEMO:-}" != "1" ]]; then
      echo "run demo is restricted to dev-demo; use 'bash infra/dev/dev.sh dev-demo'" >&2
      exit 64
    fi
    run_with_workspace_lock
    exec python3 infra/dev/demo.py --inside-container ;;
  build)
    run_with_workspace_lock
    case "${2:-server}" in
      server) cargo build --locked -p openpush-server ;;
      web) corepack pnpm install --frozen-lockfile && corepack pnpm --filter @openpush/desktop build ;;
      *) usage ;;
    esac ;;
  test)
    run_with_workspace_lock
    case "${2:-rust}" in
      rust) cargo test --workspace --locked ;;
      web) corepack pnpm install --frozen-lockfile && corepack pnpm --filter @openpush/desktop-ui test && corepack pnpm --filter @openpush/desktop test ;;
      *) usage ;;
    esac ;;
  *) usage ;;
esac
