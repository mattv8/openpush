#!/usr/bin/env bash
set -euo pipefail

if (($# != 1)); then echo "usage: $0 <bin-dir>" >&2; exit 2; fi
bin_dir=$1
version=2.14.2
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) triple=x86_64-unknown-linux-gnu; checksum=26d1f7c8ea2400f2ccb0b2e7f321635f600b583bf4cd1f7afaa5e8c24068ad1088a2504e0a4d9f4333d4bb20a80329327d9462634343ee3af91e1a7fbdc6f18a ;;
    Darwin-arm64) triple=aarch64-apple-darwin; checksum=6ec30660233fcd73b92e9d888965945b6435d6fcd7120df8d7c0799faa46cf06d563d49de721ccf4387e6d61bdc5b37c8eb73273c7d9a0883b90cd9857c6ad9f ;;
    *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
if [[ -x "$bin_dir/git-cliff" ]] && [[ $("$bin_dir/git-cliff" --version | awk '{print $2}') == "$version" ]]; then exit 0; fi
mkdir -p "$bin_dir"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
archive="$tmp/git-cliff.tar.gz"
url="https://github.com/orhun/git-cliff/releases/download/v$version/git-cliff-$version-$triple.tar.gz"
curl --fail --location --silent --show-error "$url" --output "$archive"
if command -v sha512sum >/dev/null; then actual=$(sha512sum "$archive" | awk '{print $1}'); else actual=$(shasum -a 512 "$archive" | awk '{print $1}'); fi
if [[ $actual != "$checksum" ]]; then echo "git-cliff checksum mismatch" >&2; exit 1; fi
tar -xzf "$archive" -C "$tmp"
install "$tmp/git-cliff-$version/git-cliff" "$bin_dir/git-cliff"
