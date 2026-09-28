#!/usr/bin/env bash
# Build joust in release mode and package it for distribution:
#
#   dist/joust-<version>-<target>.tar.gz          joust, README.md, LICENSE
#   dist/joust-<version>-<target>.tar.gz.sha256   its SHA-256 checksum
#
# Usage: scripts/package.sh [target]
#
# <target> is a Rust target triple (default: this machine's). Building for
# another target needs its standard library (`rustup target add <target>`) and,
# on Linux, a cross linker; the release workflow sets that up. The archive holds
# a single top-level directory named like the archive.
set -euo pipefail

cd "$(dirname "$0")/.."

target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
# `cargo pkgid` prints `path+file:///…/joust#0.1.0` (or `…#joust@0.1.0`).
version="$(cargo pkgid | sed 's/.*[#@]//')"
name="joust-${version}-${target}"

cargo build --release --locked --target "$target"

stage="target/dist/${name}"
rm -rf "$stage"
mkdir -p "$stage" dist
cp "target/${target}/release/joust" README.md LICENSE "$stage/"

# COPYFILE_DISABLE keeps macOS tar from adding `._*` resource-fork files.
COPYFILE_DISABLE=1 tar -C target/dist -czf "dist/${name}.tar.gz" "$name"

if command -v sha256sum >/dev/null; then
    sha256=(sha256sum)
else
    sha256=(shasum -a 256) # macOS
fi
(cd dist && "${sha256[@]}" "${name}.tar.gz" >"${name}.tar.gz.sha256")

echo "dist/${name}.tar.gz"
