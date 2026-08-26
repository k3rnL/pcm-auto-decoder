#!/usr/bin/env bash
set -euo pipefail

target=${1:?usage: ci-gates.sh TARGET [EXPECTED_TAG]}
expected_tag=${2:-}
target_dir=${CARGO_TARGET_DIR:-target}
binary=${target_dir}/${target}/release/pcm-auto-decoder

rustup component add rustfmt clippy
rustup target add "${target}"

cargo fmt --all -- --check
cargo clippy --locked --all-targets --target "${target}" -- -D warnings
cargo test --locked --all-targets --target "${target}"
cargo build --locked --release --target "${target}"

version_args=(--binary "${binary}")
if [[ -n "${expected_tag}" ]]; then
    version_args+=(--tag "${expected_tag}")
fi
python3 scripts/verify-version.py "${version_args[@]}"
scripts/verify-native-binary.sh "${binary}" "${target}"
scripts/smoke-offline.sh "${binary}"
