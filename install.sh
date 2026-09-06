#!/usr/bin/env bash
set -euo pipefail

readonly REPOSITORY="includeamin/drift-rs"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="${BIN_DIR:-$PREFIX/bin}"
VERSION="${VERSION:-}"

usage() {
    cat <<'EOF'
Install drift from a published GitHub release.

Usage: install.sh [--version VERSION]

Environment:
  VERSION  Release tag to install, for example v0.13.1
  PREFIX   Installation prefix (default: ~/.local)
  BIN_DIR  Executable directory (default: $PREFIX/bin)
EOF
}

while (($# > 0)); do
    case "$1" in
        --version)
            (($# >= 2)) || { printf 'error: --version requires a value\n' >&2; exit 2; }
            VERSION="$2"
            shift 2
            ;;
        --version=*)
            VERSION="${1#*=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'error: unknown option: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

command -v curl >/dev/null 2>&1 || {
    printf 'error: curl is required\n' >&2
    exit 1
}
command -v install >/dev/null 2>&1 || {
    printf 'error: install is required\n' >&2
    exit 1
}

case "$(uname -s)" in
    Linux) platform="linux" ;;
    Darwin) platform="macos" ;;
    MINGW*|MSYS*|CYGWIN*) platform="windows" ;;
    *)
        printf 'error: unsupported operating system: %s\n' "$(uname -s)" >&2
        exit 1
        ;;
esac

case "$(uname -m)" in
    x86_64|amd64) architecture="x86_64" ;;
    arm64|aarch64)
        architecture="aarch64"
        ;;
    *)
        printf 'error: unsupported architecture: %s\n' "$(uname -m)" >&2
        exit 1
        ;;
esac

if [[ "$platform" == "linux" && "$architecture" != "x86_64" ]]; then
    printf 'error: no published Linux %s binary\n' "$architecture" >&2
    exit 1
fi
if [[ "$platform" == "windows" && "$architecture" != "x86_64" ]]; then
    printf 'error: no published Windows %s binary\n' "$architecture" >&2
    exit 1
fi

if [[ "$platform" == "windows" ]]; then
    asset="drift-windows-${architecture}.exe"
else
    asset="drift-${platform}-${architecture}"
fi

if [[ -z "$VERSION" ]]; then
    VERSION="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPOSITORY/releases/latest")"
    VERSION="${VERSION##*/}"
fi
[[ "$VERSION" == v* ]] || VERSION="v$VERSION"

base_url="https://github.com/$REPOSITORY/releases/download/$VERSION"
temporary_directory="$(mktemp -d)"
trap 'rm -rf "$temporary_directory"' EXIT

printf 'Downloading drift %s for %s/%s...\n' "$VERSION" "$platform" "$architecture"
curl -fsSL --retry 3 "$base_url/$asset" -o "$temporary_directory/drift"
curl -fsSL --retry 3 "$base_url/$asset.sha256" -o "$temporary_directory/drift.sha256"

expected="$(awk '{print $1}' "$temporary_directory/drift.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$temporary_directory/drift" | awk '{print $1}')"
else
    actual="$(shasum -a 256 "$temporary_directory/drift" | awk '{print $1}')"
fi
[[ "$expected" == "$actual" ]] || {
    printf 'error: checksum verification failed\n' >&2
    exit 1
}

mkdir -p "$BIN_DIR"
if [[ "$platform" == "windows" ]]; then
    install_name="drift.exe"
else
    install_name="drift"
fi
install -m 755 "$temporary_directory/drift" "$BIN_DIR/$install_name"
printf 'installed drift %s to %s/%s\n' "$VERSION" "$BIN_DIR" "$install_name"
