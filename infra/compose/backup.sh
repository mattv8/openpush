#!/usr/bin/env bash
# Create a consistent Compose backup without deleting data or creating volumes.
set -euo pipefail

usage() { echo "usage: $0 DESTINATION_DIRECTORY" >&2; exit 64; }
test "$#" = 1 || usage
root=$(pwd -P)
destination_arg=$1
destination=$(cd "$destination_arg" && pwd -P) || { echo "destination must already exist" >&2; exit 64; }
test -f "$root/.env" || { echo ".env is required" >&2; exit 64; }
compose=(docker compose --project-directory "$root" --env-file "$root/.env" -f "$root/docker-compose.yml")
config=$(mktemp)
layout_file=$(mktemp)
state_file=$(mktemp)
trap 'rm -f "$config" "$layout_file" "$state_file"' EXIT
"${compose[@]}" config --format json > "$config"

python3 - "$config" > "$layout_file" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
want = {"postgres": {"/var/lib/postgresql"}, "seaweedfs": {"/data", "/root/.seaweedfs"}}
for service, targets in want.items():
    mounts = {v.get("target"): v for v in c["services"][service].get("volumes", []) if v.get("type") == "volume"}
    if set(mounts) & targets != targets: raise SystemExit(f"missing expected named-volume mount for {service}")
    for target in sorted(targets):
        source = mounts[target]["source"]
        print(f"{service}\t{target}\t{c['volumes'][source]['name']}")
PY
layout=()
while IFS= read -r entry; do layout[${#layout[@]}]="$entry"; done < "$layout_file"
test "${#layout[@]}" = 3 || { echo "unexpected Compose storage layout" >&2; exit 65; }
project=$(python3 - "$config" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))["name"])
PY
)
postgres_image=$(python3 - "$config" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))["services"]["postgres"]["image"])
PY
)
seaweed_image=$(python3 - "$config" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))["services"]["seaweedfs"]["image"])
PY
)

# Refuse before stopping anything when the source project/volumes are absent.
for entry in "${layout[@]}"; do
  IFS=$'\t' read -r _ _ volume <<< "$entry"
  docker volume inspect "$volume" >/dev/null || { echo "source volume is missing: $volume" >&2; exit 66; }
done
running=$("${compose[@]}" ps --status running --services || true)
grep -qx postgres <<< "$running" || { echo "source PostgreSQL must be running" >&2; exit 66; }

# Inspect all containers so clean stopped states are visible and unstable states fail closed.
"${compose[@]}" ps --all --format json > "$state_file"
python3 - "$state_file" <<'PYSTATE'
import json, sys
with open(sys.argv[1]) as source:
    text = source.read()
try:
    parsed = json.loads(text)
    services = parsed if isinstance(parsed, list) else [parsed]
except json.JSONDecodeError:
    services = [json.loads(line) for line in text.splitlines() if line.strip()]
if not services or not all(isinstance(service, dict) for service in services):
    raise SystemExit("could not parse compose state")
state_map = {s["Service"]: s["State"] for s in services if "Service" in s}
allowed = {
    "postgres": {"running"},
    "api": {"running", "created", "exited"},
    "seaweedfs": {"running", "created", "exited"},
}
for service, valid_states in allowed.items():
    state = state_map.get(service, "")
    if state and state not in valid_states:
        raise SystemExit(f"{service} is in invalid state for backup: {state}")
PYSTATE

# Compare every source container with its configured image before stopping anything.
verify_image() {
  service=$1
  configured_image=$2
  container=$("${compose[@]}" ps --all -q "$service")
  test -n "$container" || { echo "source container is missing: $service" >&2; return 67; }
  running_id=$(docker inspect --format '{{.Image}}' "$container")
  configured_id=$(docker image inspect --format '{{.Id}}' "$configured_image")
  test "$running_id" = "$configured_id" || {
    echo "$service container image mismatch (configured: $configured_image, running: $running_id)" >&2
    return 67
  }
}
verify_image postgres "$postgres_image"
verify_image seaweedfs "$seaweed_image"
api_container=$("${compose[@]}" ps --all -q api)
if test -n "$api_container"; then
  api_image=$(python3 - "$config" <<'PYIMG'
import json, sys
print(json.load(open(sys.argv[1]))["services"]["api"]["image"])
PYIMG
  )
  verify_image api "$api_image"
fi

stamp=$(date -u +%Y%m%dT%H%M%SZ)
backup_dir="$destination/openpush-${project}-${stamp}"
mkdir "$backup_dir"
api_was_running=0; seaweed_was_running=0
grep -qx api <<< "$running" && api_was_running=1
grep -qx seaweedfs <<< "$running" && seaweed_was_running=1
cleanup() {
  status=$?
  rm -f "$config" "$layout_file" "$state_file"
  if test "$seaweed_was_running" = 1 && ! "${compose[@]}" start seaweedfs >/dev/null 2>&1; then
    echo "warning: failed to restart seaweedfs after backup" >&2
  fi
  if test "$api_was_running" = 1 && ! "${compose[@]}" start api >/dev/null 2>&1; then
    echo "warning: failed to restart api after backup" >&2
  fi
  exit "$status"
}
trap cleanup EXIT HUP INT TERM

if test "$api_was_running" = 1; then
  echo "Stopping API; wait for in-flight requests to drain before running this command." >&2
  "${compose[@]}" stop api
fi
"${compose[@]}" exec -T postgres sh -ec 'exec pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" --format=custom' > "$backup_dir/postgres.dump"
if test "$seaweed_was_running" = 1; then "${compose[@]}" stop seaweedfs; fi

for entry in "${layout[@]}"; do
  IFS=$'\t' read -r service target volume <<< "$entry"
  test "$service" = seaweedfs || continue
  archive=$([[ "$target" = /data ]] && echo seaweed-data.tar.gz || echo seaweed-home.tar.gz)
  docker run --rm -v "$volume:/source:ro" -v "$backup_dir:/backup" "$postgres_image" \
    tar --sparse -C /source -czf "/backup/$archive" .
done
postgres_container=$("${compose[@]}" ps --all -q postgres)
seaweed_container=$("${compose[@]}" ps --all -q seaweedfs)
python3 - "$backup_dir/manifest.json" "$project" "$stamp" "$postgres_image" "$seaweed_image" "$postgres_container" "$seaweed_container" "${layout[@]}" <<'PY'
import json, subprocess, sys
out, project, stamp, pg, seaweed, pgc, swc, *layout = sys.argv[1:]
def image_id(container):
    return subprocess.check_output(["docker", "inspect", "--format", "{{.Image}}", container], text=True).strip()
mounts = [dict(zip(("service", "target", "volume"), item.split("\t"))) for item in layout]
json.dump({"format_version": 2, "created_at_utc": stamp, "source_project": project,
           "images": {"configured": {"postgres": pg, "seaweedfs": seaweed},
                      "running_ids": {"postgres": image_id(pgc), "seaweedfs": image_id(swc)}},
           "mounts": mounts,
           "artifacts": ["postgres.dump", "seaweed-data.tar.gz", "seaweed-home.tar.gz"]}, open(out, "w"), sort_keys=True)
PY
( cd "$backup_dir" && shasum -a 256 manifest.json postgres.dump seaweed-data.tar.gz seaweed-home.tar.gz > manifest.sha256 )
echo "Backup written to $backup_dir" >&2
