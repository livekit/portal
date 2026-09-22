#!/usr/bin/env bash
#
# Install a clang new enough to build webrtc-sys on Linux.
#
# webrtc-sys 0.3.44+ compiles against the hermetic libc++ shipped inside the
# libwebrtc artifact. That libc++ tracks LLVM trunk, and webrtc-sys/build.rs
# refuses anything older than the clang it was built with (21 at the time of
# writing). The GitHub runner images ship clang 18 and no apt package is new
# enough, so this pulls the official LLVM release tarball instead.
#
# Adapted from livekit/rust-sdks .github/scripts/install-clang.sh.
#
# Exports CC/CXX via $GITHUB_ENV when running as a workflow step, so every
# later step (cargo build, build_ffi_python.sh) picks the new compiler up.
# Prints the bin directory on stdout for use outside Actions.

set -euo pipefail

LLVM_VERSION="${LLVM_VERSION:-21.1.8}"
LLVM_ROOT="${LLVM_ROOT:-/opt/llvm-$LLVM_VERSION}"

case "$(uname -m)" in
  x86_64)          llvm_arch=X64 ;;
  aarch64 | arm64) llvm_arch=ARM64 ;;
  *) echo "install-clang.sh: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

if [ "$(id -u)" -eq 0 ]; then
  sudo=""
else
  sudo="sudo"
fi

if [ ! -x "$LLVM_ROOT/bin/clang++" ]; then
  url="https://github.com/llvm/llvm-project/releases/download/llvmorg-$LLVM_VERSION/LLVM-$LLVM_VERSION-Linux-$llvm_arch.tar.xz"
  echo "install-clang.sh: fetching $url" >&2
  $sudo mkdir -p "$LLVM_ROOT"
  # --strip-components=1 drops the LLVM-<version>-Linux-<arch>/ prefix.
  curl --fail --location --silent --show-error "$url" \
    | $sudo tar -xJ --strip-components=1 -C "$LLVM_ROOT"
fi

"$LLVM_ROOT/bin/clang++" --version >&2

if [ -n "${GITHUB_ENV:-}" ]; then
  {
    echo "CC=$LLVM_ROOT/bin/clang"
    echo "CXX=$LLVM_ROOT/bin/clang++"
  } >> "$GITHUB_ENV"
fi

echo "$LLVM_ROOT/bin"
