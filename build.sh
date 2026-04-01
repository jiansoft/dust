#!/usr/bin/env bash
set -euo pipefail

BIN_NAME="dust"
PROFILE="${PROFILE:-release}"
TARGETS="${TARGETS:-}"
BUILD_COUNT=0
START_TS="$(date +%s)"

if [[ "${PROFILE}" != "release" && "${PROFILE}" != "debug" ]]; then
  echo "Unsupported PROFILE: ${PROFILE}"
  echo "Use PROFILE=release or PROFILE=debug"
  exit 1
fi

echo "[1/5] Checking Rust toolchain..."
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is not installed or not in PATH."
  echo "Install Rust first: https://rustup.rs"
  exit 1
fi

if ! command -v rustc >/dev/null 2>&1; then
  echo "rustc is not installed or not in PATH."
  echo "Install Rust first: https://rustup.rs"
  exit 1
fi

echo "[2/5] Tool versions:"
echo "  - $(rustc --version)"
echo "  - $(cargo --version)"

echo "[3/5] Fetching dependencies..."
cargo fetch

build_target() {
  local target="$1"
  local target_flag=()
  local output_path=""

  BUILD_COUNT=$((BUILD_COUNT + 1))
  echo
  if [[ -n "${target}" ]]; then
    echo "[4/5] Build ${BUILD_COUNT}: ${target}"
    rustup target add "${target}"
    target_flag=(--target "${target}")
    output_path="target/${target}/${PROFILE}/${BIN_NAME}"
  else
    echo "[4/5] Build ${BUILD_COUNT}: host target"
    output_path="target/${PROFILE}/${BIN_NAME}"
  fi

  local build_start
  build_start="$(date +%s)"
  cargo build "${target_flag[@]}" "--${PROFILE}"
  local build_end
  build_end="$(date +%s)"

  if [[ ! -f "${output_path}" ]]; then
    echo "Build finished, but binary was not found: ${output_path}"
    exit 1
  fi

  local elapsed=$((build_end - build_start))
  printf 'Output binary: %s\n' "${output_path}"
  printf 'Elapsed: %02d:%02d:%02d\n' "$((elapsed / 3600))" "$(((elapsed % 3600) / 60))" "$((elapsed % 60))"
}

if [[ -n "${TARGETS}" ]]; then
  for target in ${TARGETS}; do
    build_target "${target}"
  done
else
  build_target ""
fi

TOTAL_ELAPSED="$(( $(date +%s) - START_TS ))"
echo
echo "[5/5] Done."
echo "Targets built: ${BUILD_COUNT}"
printf 'Total build time: %02d:%02d:%02d\n' "$((TOTAL_ELAPSED / 3600))" "$(((TOTAL_ELAPSED % 3600) / 60))" "$((TOTAL_ELAPSED % 60))"
