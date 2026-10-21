#!/bin/sh
# velmux installer: curl -fsSL https://velmux.com/install.sh | sh
#
# Downloads the latest release for this machine from GitHub, checks it against
# the release's checksum, and puts `velmux` in $VELMUX_INSTALL_DIR (default
# ~/.local/bin). Nothing else on the system is touched.
set -eu

repo=vincentvella/velmux
dir=${VELMUX_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) echo "velmux runs on macOS and Linux (this is $(uname -s))" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) echo "no velmux build for $(uname -m)" >&2; exit 1 ;;
esac
target=$arch-$os
file=velmux-$target.tar.gz
url=https://github.com/$repo/releases/latest/download

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "downloading velmux for $target"
curl -fsSL "$url/$file" -o "$tmp/$file"
curl -fsSL "$url/$file.sha256" -o "$tmp/$file.sha256"
expected=$(cut -d' ' -f1 < "$tmp/$file.sha256")
if command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$tmp/$file" | cut -d' ' -f1)
else
  actual=$(sha256sum "$tmp/$file" | cut -d' ' -f1)
fi
[ "$expected" = "$actual" ] || { echo "checksum mismatch for $file" >&2; exit 1; }

tar -xzf "$tmp/$file" -C "$tmp"
mkdir -p "$dir"
mv "$tmp"/velmux-*/velmux "$dir/velmux"
chmod +x "$dir/velmux"
echo "installed $("$dir/velmux" --version) to $dir/velmux"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "note: $dir isn't on your PATH; add it to your shell's profile" ;;
esac
echo
echo "next:"
echo "  velmux skill install   # teach Claude to work in velmux"
echo "  cd your-repo && velmux"
