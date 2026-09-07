#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

case "${1:-full}" in
    full|platform) mode=${1:-full} ;;
    *) echo 'usage: sh scripts/verify.sh [full|platform]' >&2; exit 2 ;;
esac

python_bin=${PYTHON:-python3}
"$python_bin" -B scripts/test_check_privacy.py
"$python_bin" -B scripts/check_privacy.py
"$python_bin" -B scripts/check_privacy.py --staged
git diff --check
git diff --cached --check

if [ "$mode" = full ]; then
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
fi
cargo build --locked --all-features
cargo test --locked --all-features
