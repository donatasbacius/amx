#!/bin/sh
# Install the latest amx release binary.
# Usage: curl -fsSL https://raw.githubusercontent.com/donatasbacius/amx/main/install.sh | sh
#        sh install.sh [--prefix DIR]   (installs to DIR/bin, default ~/.local)
set -eu

REPO_URL="${AMX_REPO_URL:-https://github.com/donatasbacius/amx}"

prefix="${HOME}/.local"
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) [ $# -ge 2 ] || { echo "--prefix needs a directory" >&2; exit 2; }; prefix="$2"; shift 2 ;;
        --prefix=*) prefix="${1#--prefix=}"; shift ;;
        -h|--help) echo "usage: install.sh [--prefix DIR]"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux)
        case "$arch" in
            x86_64|amd64) asset="amx-linux-x86_64" ;;
            aarch64|arm64) asset="amx-linux-aarch64" ;;
            *) echo "unsupported Linux architecture: $arch" >&2; exit 1 ;;
        esac
        ;;
    Darwin)
        case "$arch" in
            x86_64|arm64) asset="amx-macos-universal" ;;
            *) echo "unsupported macOS architecture: $arch" >&2; exit 1 ;;
        esac
        ;;
    *) echo "unsupported OS: $os (try: cargo install --git $REPO_URL)" >&2; exit 1 ;;
esac

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        echo "need curl or wget" >&2
        exit 1
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        echo "need sha256sum or shasum" >&2
        exit 1
    fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

base="$REPO_URL/releases/latest/download"
echo "downloading $asset"
fetch "$base/$asset" "$tmp/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"

expected="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
actual="$(sha256 "$tmp/$asset")"
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    echo "checksum mismatch for $asset (expected '${expected:-none}', got '$actual')" >&2
    exit 1
fi

bindir="$prefix/bin"
mkdir -p "$bindir"
chmod 755 "$tmp/$asset"
mv "$tmp/$asset" "$bindir/amx"
echo "installed $bindir/amx ($("$bindir/amx" --version))"
case ":${PATH}:" in
    *":$bindir:"*) ;;
    *) echo "note: $bindir is not on your PATH" ;;
esac
