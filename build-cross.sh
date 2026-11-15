#!/usr/bin/env bash
# Cross-compile from Linux (what was used to produce the release zip).
# On Windows just run:  cargo build --release
set -euo pipefail
RUSTC_BOOTSTRAP=1 cargo +${TOOLCHAIN:-rs191w} build --release \
  -Zbuild-std=std,panic_unwind --target x86_64-pc-windows-gnu "$@"
