#!/usr/bin/env bash

set -Eeuo pipefail

JOBS=(
  changes source-audit version release-tooling development-tools rust integration desktop android
  swift-bindings container-image development-artifacts
)

prefix_for() {
  printf '%s\n' "$1" | tr '[:lower:]-' '[:upper:]_'
}

require_env() {
  local name="$1"

  if [ -z "${!name+x}" ] || [ -z "${!name}" ]; then
    echo "missing required environment variable: $name" >&2
    exit 1
  fi
}

validate_bool() {
  case "$2" in
    true|false) ;;
    *)
      echo "$1 must be true or false" >&2
      exit 1
      ;;
  esac
}

validate_result() {
  case "$2" in
    success|skipped|failure|cancelled) ;;
    *)
      echo "$1 must be a recognized GitHub result" >&2
      exit 1
      ;;
  esac
}

write_summary() {
  local job prefix result_var required_var

  if [ -z "${GITHUB_STEP_SUMMARY:-}" ]; then
    return
  fi

  {
    printf '| job | required | result |\n'
    printf '| --- | --- | --- |\n'
    for job in "${JOBS[@]}"; do
      prefix="$(prefix_for "$job")"
      result_var="${prefix}_RESULT"
      required_var="${prefix}_REQUIRED"
      printf '| %s | %s | %s |\n' "$job" "${!required_var:--}" "${!result_var:--}"
    done
  } >> "$GITHUB_STEP_SUMMARY" 2>/dev/null || true
}

validate_job_inputs() {
  local job="$1"
  local prefix result_var required_var

  prefix="$(prefix_for "$job")"
  result_var="${prefix}_RESULT"
  required_var="${prefix}_REQUIRED"
  require_env "$result_var"
  require_env "$required_var"
  validate_result "$result_var" "${!result_var}"
  validate_bool "$required_var" "${!required_var}"

}

validate_job_result() {
  local job="$1"
  local prefix result_var required_var

  prefix="$(prefix_for "$job")"
  result_var="${prefix}_RESULT"
  required_var="${prefix}_REQUIRED"

  if [ "$job" = changes ]; then
    if [ "${!result_var}" != success ]; then
      echo "$job returned ${!result_var}" >&2
      exit 1
    fi
    return
  fi

  if [ "${!result_var}" = success ] || {
    [ "${!result_var}" = skipped ] && [ "${!required_var}" = false ]
  }; then
    return
  fi

  echo "$job returned ${!result_var} with required=${!required_var}" >&2
  exit 1
}

for job in "${JOBS[@]}"; do
  validate_job_inputs "$job"
done

write_summary

for job in "${JOBS[@]}"; do
  validate_job_result "$job"
done
