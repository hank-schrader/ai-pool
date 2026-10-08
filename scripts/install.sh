#!/bin/sh
# Install ai-pool (pool-miner, pool-server, clef-token-count) from a GitHub release
# for the current user. Linux x86_64 and macOS Apple Silicon.
#
#   curl -fsSL https://raw.githubusercontent.com/hank-schrader/ai-pool/main/scripts/install.sh | sh
#   sh install.sh --version v0.1.0
#   sh install.sh --no-modify-path     # leave shell startup files alone
#
# Installs into ${XDG_DATA_HOME:-~/.local/share}/ai-pool/<version>, links the
# binaries into ~/.local/bin and adds that to PATH in your shell's startup file.
# Nothing is installed system-wide; GPU drivers are not touched.
set -eu

repo="hank-schrader/ai-pool"
version="latest"
data="${XDG_DATA_HOME:-$HOME/.local/share}/ai-pool"
bin="$HOME/.local/bin"
modify_path=1

while [ $# -gt 0 ]; do
    case "$1" in
        --version) version="$2"; shift 2 ;;
        --bin-dir) bin="$2"; shift 2 ;;
        --no-modify-path) modify_path=0; shift ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
    Darwin-arm64) target="aarch64-apple-darwin" ;;
    *) echo "unsupported platform: $(uname -s) $(uname -m) (supported: Linux x86_64, macOS arm64)" >&2; exit 1 ;;
esac

if [ "$version" = latest ]; then
    version=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
    [ -n "$version" ] || { echo "could not find the latest release" >&2; exit 1; }
fi

name="ai-pool-$version-$target"
base="https://github.com/$repo/releases/download/$version"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "downloading $name"
curl -fL --progress-bar -o "$tmp/$name.tar.gz" "$base/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

expected=$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null; then actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
else actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1); fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || { echo "checksum mismatch for $name.tar.gz" >&2; exit 1; }

mkdir -p "$data/$version" "$bin"
tar xzf "$tmp/$name.tar.gz" -C "$data/$version"
for tool in pool-miner pool-server; do
    ln -sf "$data/$version/$name/$tool" "$bin/$tool"
done

echo "installed $version to $data/$version/$name"

# Persist $bin on PATH in the login shell's startup file, once.
case ":$PATH:" in
    *":$bin:"*) ;;
    *)
        if [ "$modify_path" = 1 ]; then
            line="export PATH=\"$bin:\$PATH\""
            case "$(basename "${SHELL:-sh}")" in
                zsh) rc="${ZDOTDIR:-$HOME}/.zshrc" ;;
                bash) if [ "$(uname -s)" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi ;;
                fish) rc="${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/ai-pool.fish"; line="fish_add_path -g \"$bin\"" ;;
                *) rc="$HOME/.profile" ;;
            esac
            if [ ! -f "$rc" ] || ! grep -qF "$line" "$rc"; then
                mkdir -p "$(dirname "$rc")"
                printf '\n# added by the ai-pool installer\n%s\n' "$line" >> "$rc"
                echo "added $bin to PATH in $rc"
            else
                echo "$bin is already on PATH in $rc"
            fi
            echo "open a new terminal (or run: . $rc) to use pool-miner"
        else
            echo "add $bin to your PATH to use pool-miner"
        fi
        ;;
esac
echo "next: pool-miner --pool https://<pool-host>"
