#!/usr/bin/env bash
# shellcheck disable=SC2068

# shellcheck source=/dev/null
source "$HOME"/.cargo/env
source "$(dirname "$0")"/test-util.sh

process_common_args "$@"

cargo_args=("")

# shellcheck disable=SC2154
if [[ $hypervisor = "mshv" ]]; then
    cargo_args+=("--features $hypervisor")
fi

if ! command -v clang >/dev/null 2>&1 || [[ $BUILD_TARGET == *-musl ]]; then
    cargo_args+=("--exclude scx-pylon --exclude pylond")
fi

export RUST_BACKTRACE=1
cargo test --lib --bins --target "$BUILD_TARGET" --release --workspace ${cargo_args[@]} || exit 1
cargo test --doc --target "$BUILD_TARGET" --release --workspace ${cargo_args[@]} || exit 1
