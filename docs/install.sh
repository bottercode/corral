#!/bin/sh
# Install corral, a terminal runtime for coding agents.
#
#   curl -fsSL https://bottercode.github.io/corral/install.sh | sh
#
# Fetches the installer of the latest release for this machine (macOS or
# Linux, Intel/AMD or ARM) and runs it; it puts `corral` in ~/.cargo/bin and
# adds that to your PATH if needed. Arguments are passed through to it.
# Source and releases: https://github.com/bottercode/corral
set -eu

url="https://github.com/bottercode/corral/releases/latest/download/corral-agents-installer.sh"

case "$(uname -s)" in
    Darwin | Linux) ;;
    *)
        echo "corral runs on macOS and Linux; $(uname -s) isn't supported yet." >&2
        exit 1
        ;;
esac

tmp="$(mktemp "${TMPDIR:-/tmp}/corral-install.XXXXXX")"
trap 'rm -f "$tmp"' EXIT INT TERM

if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$tmp"
elif command -v wget >/dev/null 2>&1; then
    wget --https-only -qO "$tmp" "$url"
else
    echo "corral's installer needs curl or wget." >&2
    exit 1
fi

sh "$tmp" "$@"
