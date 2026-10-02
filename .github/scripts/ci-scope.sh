#!/usr/bin/env bash

set -Eeuo pipefail

: "${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}"

normalize_bool() {
  local name="$1"
  local value="${2:-}"

  if [ -z "$value" ]; then
    printf 'false\n'
    return
  fi

  case "$value" in
    true|false) printf '%s\n' "$value" ;;
    *)
      echo "invalid boolean for $name: $value" >&2
      exit 1
      ;;
  esac
}

bool_or() {
  local value

  for value in "$@"; do
    if [ "$value" = true ]; then
      printf 'true\n'
      return
    fi
  done

  printf 'false\n'
}

FORCE_FULL="$(normalize_bool FILTER_FORCE_FULL "${FILTER_FORCE_FULL:-}")"
RUST_FILTER="$(normalize_bool FILTER_RUST "${FILTER_RUST:-}")"
DESKTOP_FILTER="$(normalize_bool FILTER_DESKTOP "${FILTER_DESKTOP:-}")"
ANDROID_FILTER="$(normalize_bool FILTER_ANDROID "${FILTER_ANDROID:-}")"
IOS_FILTER="$(normalize_bool FILTER_IOS "${FILTER_IOS:-}")"
SERVER_FILTER="$(normalize_bool FILTER_SERVER "${FILTER_SERVER:-}")"
BUILD_FILTER="$(normalize_bool FILTER_BUILD "${FILTER_BUILD:-}")"
CI_FILTER="$(normalize_bool FILTER_CI "${FILTER_CI:-}")"
UNCLASSIFIED_FILTER="$(normalize_bool FILTER_UNCLASSIFIED "${FILTER_UNCLASSIFIED:-}")"

ALL="$(bool_or "$FORCE_FULL" "$CI_FILTER" "$UNCLASSIFIED_FILTER")"
RUST="$(bool_or "$ALL" "$RUST_FILTER")"
INTEGRATION="$(bool_or "$ALL" "$RUST_FILTER" "$DESKTOP_FILTER" "$SERVER_FILTER")"
DESKTOP="$(bool_or "$ALL" "$RUST_FILTER" "$DESKTOP_FILTER")"
ANDROID="$(bool_or "$ALL" "$RUST_FILTER" "$ANDROID_FILTER")"
SWIFT_BINDINGS="$(bool_or "$ALL" "$RUST_FILTER" "$IOS_FILTER" "$ANDROID_FILTER")"
CONTAINER_IMAGE="$(bool_or "$ALL" "$RUST_FILTER" "$SERVER_FILTER")"
DEVELOPMENT_ARTIFACTS="$(bool_or "$ALL" "$RUST_FILTER" "$DESKTOP_FILTER" "$ANDROID_FILTER" "$IOS_FILTER" "$BUILD_FILTER")"

cat >> "$GITHUB_OUTPUT" <<EOF
rust=$RUST
integration=$INTEGRATION
desktop=$DESKTOP
android=$ANDROID
swift_bindings=$SWIFT_BINDINGS
container_image=$CONTAINER_IMAGE
development_artifacts=$DEVELOPMENT_ARTIFACTS
EOF
