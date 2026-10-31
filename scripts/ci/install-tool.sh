#!/usr/bin/env bash
# Install a pinned CI tool from its GitHub release, checked against a SHA-256
# kept here, instead of `curl ... | sh` or a slow `cargo install`.
# Same script as hiranp/hief and hiranp/email-agent.
#
#   scripts/ci/install-tool.sh cargo-nextest [dest-dir]   (default ~/.cargo/bin)
#
# Linux x86_64 and aarch64 only (CI runners). On macOS use
# `brew install cargo-nextest`; scripts/local-ci/ci.sh says so if it is missing.
# To bump a tool, change its version and both hashes together.
set -euo pipefail

tool="${1:?usage: install-tool.sh <tool> [dest-dir]}"
dest="${2:-$HOME/.cargo/bin}"

[[ "$(uname -s)" == Linux ]] || { echo "install-tool: Linux only; on macOS: brew install $tool" >&2; exit 1; }
case "$(uname -m)" in
  x86_64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) echo "install-tool: unsupported arch $(uname -m)" >&2; exit 1 ;;
esac

case "$tool" in
  cargo-nextest)
    # No .sha256 files are published; these are sha256sum of the release tarballs.
    version=0.9.149
    url="https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-${version}/cargo-nextest-${version}-${arch}-unknown-linux-musl.tar.gz"
    case "$arch" in
      x86_64) sha=d65c7f14bcb5c1566d1ecd534c5da8dedb81ba58586e478b0ab81c877d2bd8ad ;;
      aarch64) sha=533c9b431c2bb1dcb216e02e474263db0cdcd556373b72cc34830fb417058a6b ;;
    esac
    ;;
  *) echo "install-tool: unknown tool $tool" >&2; exit 1 ;;
esac

if command -v "$tool" >/dev/null 2>&1 && "$tool" nextest --version 2>/dev/null | grep -qF "$version"; then
  echo "install-tool: $tool $version already installed"
  exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$tmp/t.tar.gz" "$url"
echo "$sha  $tmp/t.tar.gz" | sha256sum -c --quiet -
tar -xzf "$tmp/t.tar.gz" -C "$tmp"
mkdir -p "$dest"
install -m 0755 "$tmp/$tool" "$dest/$tool"
"$dest/$tool" nextest --version
