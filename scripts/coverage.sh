#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COVERAGE_DIR="${ROOT_DIR}/coverage"
RUST_DIR="${COVERAGE_DIR}/rust"
PYTHON_DIR="${COVERAGE_DIR}/python"

usage() {
  cat <<'EOF'
Usage: scripts/coverage.sh [--rust-only | --python-only]

Generates repository coverage reports for:
- Rust workspace crates via cargo-llvm-cov
- Harness sandbox Python tests via python -m trace
- Telegram connector tests via uv + pytest-cov

Reports are written under ./coverage/
EOF
}

mode="all"
if [[ $# -gt 1 ]]; then
  usage
  exit 1
elif [[ $# -eq 1 ]]; then
  case "$1" in
    --rust-only)
      mode="rust"
      ;;
    --python-only)
      mode="python"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 1
      ;;
  esac
fi

mkdir -p "${RUST_DIR}" "${PYTHON_DIR}"

run_rust_coverage() {
  if ! command -v cargo-llvm-cov >/dev/null 2>&1 && ! cargo llvm-cov --version >/dev/null 2>&1; then
    cat >&2 <<'EOF'
error: cargo-llvm-cov is required for Rust coverage.

Install it with:
  cargo install cargo-llvm-cov

If you also need LLVM tooling:
  rustup component add llvm-tools-preview
EOF
    exit 1
  fi

  echo "==> Generating Rust workspace coverage"
  rm -rf "${RUST_DIR}"
  mkdir -p "${RUST_DIR}"
  cargo llvm-cov clean --workspace
  cargo llvm-cov \
    --workspace \
    --all-features \
    --html \
    --output-dir "${RUST_DIR}"
  cargo llvm-cov report \
    --lcov \
    --output-path "${RUST_DIR}/lcov.info"
}

run_harness_python_coverage() {
  echo "==> Generating harness sandbox trace coverage"
  rm -rf "${PYTHON_DIR}/harness-sandbox"
  mkdir -p "${PYTHON_DIR}/harness-sandbox"
  (
    cd "${ROOT_DIR}"
    python3 -m trace \
      --count \
      --coverdir "${PYTHON_DIR}/harness-sandbox" \
      -m unittest harness/sandbox/test_runner.py
  )
}

run_connector_python_coverage() {
  if ! command -v uv >/dev/null 2>&1; then
    cat >&2 <<'EOF'
error: uv is required for connector Python coverage.

Install it with:
  curl -LsSf https://astral.sh/uv/install.sh | sh
EOF
    exit 1
  fi

  echo "==> Generating telegram connector pytest coverage"
  rm -rf "${PYTHON_DIR}/telegram-acp"
  mkdir -p "${PYTHON_DIR}/telegram-acp"
  (
    cd "${ROOT_DIR}/connectors/telegram-acp"
    uv run --group dev pytest \
      --cov \
      --cov-report="xml:${PYTHON_DIR}/telegram-acp/coverage.xml" \
      --cov-report="html:${PYTHON_DIR}/telegram-acp/html"
  )
}

write_summary() {
  cat > "${COVERAGE_DIR}/README.md" <<EOF
# Coverage Reports

- Rust workspace HTML report: [rust/html/index.html](rust/html/index.html)
- Rust LCOV report: [rust/lcov.info](rust/lcov.info)
- Harness sandbox trace output: [python/harness-sandbox](python/harness-sandbox)
- Telegram connector HTML report: [python/telegram-acp/html/index.html](python/telegram-acp/html/index.html)
- Telegram connector XML report: [python/telegram-acp/coverage.xml](python/telegram-acp/coverage.xml)
EOF
}

case "${mode}" in
  rust)
    (
      cd "${ROOT_DIR}"
      run_rust_coverage
    )
    ;;
  python)
    run_harness_python_coverage
    run_connector_python_coverage
    ;;
  all)
    (
      cd "${ROOT_DIR}"
      run_rust_coverage
    )
    run_harness_python_coverage
    run_connector_python_coverage
    ;;
esac

write_summary
echo "Coverage reports written to ${COVERAGE_DIR}"
