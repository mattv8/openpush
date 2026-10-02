#!/usr/bin/env bash

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCOPE="$SCRIPT_DIR/ci-scope.sh"
GATE="$SCRIPT_DIR/ci-gate.sh"
temp_dir="$(mktemp -d)"
trap 'rm -rf "$temp_dir"' EXIT

expect_output() {
  local file="$1"
  local expected="$2"

  if ! grep -Fqx "$expected" "$file"; then
    echo "missing output: $expected" >&2
    exit 1
  fi
}

expect_failure() {
  if "$@"; then
    echo "expected command to fail: $*" >&2
    exit 1
  fi
}

run_scope() {
  local output="$temp_dir/scope-output"

  : > "$output"
  env GITHUB_OUTPUT="$output" "$@" "$SCOPE"
  printf '%s\n' "$output"
}

for case_name in force_full docs_only rust desktop ios ci; do
  output="$temp_dir/$case_name"
  : > "$output"
  case "$case_name" in
    force_full) env GITHUB_OUTPUT="$output" FILTER_FORCE_FULL=true "$SCOPE" ;;
    docs_only) env GITHUB_OUTPUT="$output" "$SCOPE" ;;
    rust) env GITHUB_OUTPUT="$output" FILTER_RUST=true "$SCOPE" ;;
    desktop) env GITHUB_OUTPUT="$output" FILTER_DESKTOP=true "$SCOPE" ;;
    ios) env GITHUB_OUTPUT="$output" FILTER_IOS=true "$SCOPE" ;;
    ci) env GITHUB_OUTPUT="$output" FILTER_CI=true "$SCOPE" ;;
  esac
done

for name in rust integration desktop android swift_bindings container_image development_artifacts; do
  expect_output "$temp_dir/force_full" "$name=true"
  expect_output "$temp_dir/docs_only" "$name=false"
  expect_output "$temp_dir/rust" "$name=true"
  expect_output "$temp_dir/ci" "$name=true"
done

expect_output "$temp_dir/desktop" 'rust=false'
expect_output "$temp_dir/desktop" 'integration=true'
expect_output "$temp_dir/desktop" 'desktop=true'
expect_output "$temp_dir/desktop" 'android=false'
expect_output "$temp_dir/desktop" 'swift_bindings=false'
expect_output "$temp_dir/desktop" 'container_image=false'
expect_output "$temp_dir/desktop" 'development_artifacts=true'
expect_output "$temp_dir/ios" 'rust=false'
expect_output "$temp_dir/ios" 'swift_bindings=true'
expect_output "$temp_dir/ios" 'development_artifacts=true'
expect_failure env GITHUB_OUTPUT="$temp_dir/invalid" FILTER_RUST=invalid "$SCOPE"

gate_env=(
  CHANGES_RESULT=success CHANGES_REQUIRED=true
  SOURCE_AUDIT_RESULT=success SOURCE_AUDIT_REQUIRED=true
  VERSION_RESULT=success VERSION_REQUIRED=true
  RELEASE_TOOLING_RESULT=success RELEASE_TOOLING_REQUIRED=true
  DEVELOPMENT_TOOLS_RESULT=success DEVELOPMENT_TOOLS_REQUIRED=true
  RUST_RESULT=success RUST_REQUIRED=true
  INTEGRATION_RESULT=success INTEGRATION_REQUIRED=true
  DESKTOP_RESULT=success DESKTOP_REQUIRED=true
  ANDROID_RESULT=success ANDROID_REQUIRED=true
  SWIFT_BINDINGS_RESULT=success SWIFT_BINDINGS_REQUIRED=true
  CONTAINER_IMAGE_RESULT=success CONTAINER_IMAGE_REQUIRED=true
  DEVELOPMENT_ARTIFACTS_RESULT=success DEVELOPMENT_ARTIFACTS_REQUIRED=true
)

env "${gate_env[@]}" "$GATE"
env "${gate_env[@]}" RUST_RESULT=skipped RUST_REQUIRED=false "$GATE"
expect_failure env "${gate_env[@]}" RUST_RESULT=skipped RUST_REQUIRED=true "$GATE"
expect_failure env "${gate_env[@]}" RUST_RESULT=failure "$GATE"
expect_failure env "${gate_env[@]}" CHANGES_RESULT=failure "$GATE"
expect_failure env "${gate_env[@]}" RUST_RESULT=invalid "$GATE"
expect_failure env CHANGES_RESULT=success "$GATE"

echo 'CI script tests passed'
