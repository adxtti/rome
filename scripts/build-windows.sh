#!/usr/bin/env bash
# Cross-build the Windows release binaries from macOS (or Linux) with cargo-xwin.
#
# Usage: scripts/build-windows.sh [arm64] [x64]      (default: both)
#
# Produces dist/u_rome-windows-<arch>.zip with u_rome_studio.exe, rome.exe,
# LICENSE-MIT and LICENSE-APACHE at the zip root. dist/ is git-ignored.
#
# Requirements: rustup, cargo-xwin (`cargo install --locked cargo-xwin`),
# zip, and clang/lld (Xcode command line tools or Homebrew llvm). The first
# run downloads the MSVC CRT and Windows SDK into cargo-xwin's cache, which
# means accepting the Microsoft license (cargo-xwin does this non-interactively).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

die() {
    echo "error: $*" >&2
    exit 1
}

command -v cargo >/dev/null || die "cargo not found (install rustup)"
cargo xwin --version >/dev/null 2>&1 || die "cargo-xwin not found: cargo install --locked cargo-xwin"
command -v zip >/dev/null || die "zip not found"

archs=("$@")
if [ ${#archs[@]} -eq 0 ]; then
    archs=(arm64 x64)
fi

mkdir -p dist
for arch in "${archs[@]}"; do
    case "$arch" in
    arm64) target=aarch64-pc-windows-msvc ;;
    x64) target=x86_64-pc-windows-msvc ;;
    *) die "unknown arch '$arch' (expected arm64 or x64)" ;;
    esac

    echo "==> building $target"
    rustup target add "$target" >/dev/null
    cargo xwin build --release --locked --target "$target" -p u_rome_studio -p u_rome_cli

    stage="$(mktemp -d)"
    trap 'rm -rf "$stage"' EXIT
    cp "target/$target/release/u_rome_studio.exe" "target/$target/release/rome.exe" "$stage/"
    cp LICENSE-MIT LICENSE-APACHE "$stage/"

    zipfile="$root/dist/u_rome-windows-$arch.zip"
    rm -f "$zipfile"
    (cd "$stage" && zip -X -q "$zipfile" u_rome_studio.exe rome.exe LICENSE-MIT LICENSE-APACHE)
    rm -rf "$stage"
    trap - EXIT

    echo "==> $zipfile"
    unzip -l "$zipfile"
done
