#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
BINARY="${POLYTREAD_BIN:-${REPO_DIR}/target/release/polytread}"

if [[ ! -x "${BINARY}" ]]; then
  echo "ERROR: build the release binary first: cargo build --locked --release" >&2
  exit 1
fi

exec "${BINARY}" serve
