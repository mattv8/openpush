#!/usr/bin/env bash
# Restore only into a fresh, distinct Compose project. Existing volumes are never changed.
set -euo pipefail

usage() { echo "usage: $0 BACKUP_DIRECTORY NEW_COMPOSE_PROJECT" >&2; exit 64; }
test "$#" = 2 || usage
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 64; }
root=$(pwd -P)
archive_arg=$1
archive=$(cd "$archive_arg" && pwd -P) || { echo "backup directory must exist" >&2; exit 64; }
target_project_arg=$2
target_project=$target_project_arg
case "$target_project" in *[!a-z0-9_-]*|''|[-_]* ) echo "project name must start with lowercase letter/digit and use lowercase letters, digits, _ or -" >&2; exit 64;; esac
test -f "$root/.env" || { echo ".env is required" >&2; exit 64; }
test -f "$archive/manifest.json" && test -f "$archive/manifest.sha256" || { echo "invalid backup directory" >&2; exit 64; }

python3 - "$archive" <<'PY'
import json, os, re, sys
d=sys.argv[1]; expected={"manifest.json", "manifest.sha256", "postgres.dump", "seaweed-data.tar.gz", "seaweed-home.tar.gz"}
if set(os.listdir(d)) != expected: raise SystemExit("backup has an unexpected or missing artifact")
m=json.load(open(os.path.join(d,"manifest.json")))
artifacts=expected-{ "manifest.json", "manifest.sha256"}
if m.get("format_version") != 2 or set(m.get("artifacts",[])) != artifacts: raise SystemExit("unsupported backup manifest")
checksum_entries=[]
with open(os.path.join(d, "manifest.sha256"), encoding="ascii") as checksums:
    for line in checksums.read().splitlines():
        match=re.fullmatch(r"[0-9a-fA-F]{64}  (manifest\.json|postgres\.dump|seaweed-data\.tar\.gz|seaweed-home\.tar\.gz)", line)
        if not match: raise SystemExit("manifest.sha256 has an invalid entry")
        checksum_entries.append(match.group(1))
required={"manifest.json", "postgres.dump", "seaweed-data.tar.gz", "seaweed-home.tar.gz"}
if len(checksum_entries) != 4 or set(checksum_entries) != required:
    raise SystemExit("manifest.sha256 must list each required artifact exactly once")
PY
( cd "$archive" && shasum -a 256 -c manifest.sha256 )
source_project=$(python3 - "$archive/manifest.json" <<'PY'
import json,sys
print(json.load(open(sys.argv[1]))["source_project"])
PY
)
test "$target_project" != "$source_project" || { echo "restore target must differ from source project" >&2; exit 64; }
compose=(docker compose --project-name "$target_project" --project-directory "$root" --env-file "$root/.env" -f "$root/docker-compose.yml")
config=$(mktemp); volumes_file=$(mktemp); trap 'rm -f "$config" "$volumes_file"' EXIT
"${compose[@]}" config --format json > "$config"
python3 - "$config" "$archive/manifest.json" <<'PY'
import json,sys
c=json.load(open(sys.argv[1])); m=json.load(open(sys.argv[2]))
for service in ("postgres", "seaweedfs"):
    if c["services"][service]["image"] != m["images"]["configured"][service]: raise SystemExit(f"target {service} image does not match backup")
PY
python3 - "$config" > "$volumes_file" <<'PY'
import json,sys
c=json.load(open(sys.argv[1]))
for service, target in (("postgres","/var/lib/postgresql"),("seaweedfs","/data"),("seaweedfs","/root/.seaweedfs")):
  m=next((v for v in c["services"][service]["volumes"] if v.get("target")==target and v.get("type")=="volume"),None)
  if not m: raise SystemExit("target storage layout mismatch")
  print(c["volumes"][m["source"]]["name"])
PY
volumes=()
while IFS= read -r volume; do volumes[${#volumes[@]}]="$volume"; done < "$volumes_file"
test "${#volumes[@]}" = 3 || { echo "unexpected target storage layout" >&2; exit 65; }
for volume in "${volumes[@]}"; do
  docker volume inspect "$volume" >/dev/null 2>&1 && { echo "restore target volume already exists: $volume" >&2; exit 73; }
done

"${compose[@]}" create postgres seaweedfs >/dev/null
pg_volume=${volumes[0]}; data_volume=${volumes[1]}; home_volume=${volumes[2]}
postgres_image=$(python3 - "$config" <<'PY'
import json,sys
print(json.load(open(sys.argv[1]))["services"]["postgres"]["image"])
PY
)
docker run --rm -v "$data_volume:/destination" -v "$archive:/backup:ro" "$postgres_image" tar -C /destination -xzf /backup/seaweed-data.tar.gz
docker run --rm -v "$home_volume:/destination" -v "$archive:/backup:ro" "$postgres_image" tar -C /destination -xzf /backup/seaweed-home.tar.gz
if ! "${compose[@]}" up --wait postgres; then
  echo "PostgreSQL startup failed; restore aborted before pg_restore" >&2
  exit 74
fi
"${compose[@]}" exec -T postgres sh -ec 'pg_restore --single-transaction --exit-on-error -U "$POSTGRES_USER" -d "$POSTGRES_DB" --no-owner --no-privileges' < "$archive/postgres.dump"
echo "Restore complete for $target_project. API and migrations remain stopped for operator reconciliation." >&2
