#!/bin/sh
# Installs a prebuilt rcargo binary (no Rust toolchain needed, nothing to compile on your laptop).
#   curl -fsSL https://raw.githubusercontent.com/igun997/rcargo/main/install.sh | sh
set -eu
repo="igun997/rcargo"
os=$(uname -s); arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64)  target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64)  target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "rcargo: no prebuilt binary for $os $arch. Build from source: cargo install --git https://github.com/$repo" >&2; exit 1 ;;
esac
dest="${RCARGO_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$dest"
url="https://github.com/$repo/releases/latest/download/rcargo-$target.tar.gz"
echo "Downloading $url"
curl -fsSL "$url" | tar -xz -C "$dest" rcargo
chmod +x "$dest/rcargo"
echo "Installed $dest/rcargo"
case ":$PATH:" in *":$dest:"*) ;; *) echo "Add $dest to your PATH:  export PATH=\"$dest:\$PATH\"" ;; esac
echo "Next: go to your Rust project and run:  rcargo init"
