#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
# shellcheck disable=SC1091
. "$root/packaging/sources.env"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/ffmpeg.tar.xz" "$FFMPEG_LINUX_URL"
tar -xJf "$tmp/ffmpeg.tar.xz" -C "$tmp"
bin=$(find "$tmp" -type f -path '*/bin/ffmpeg' | head -n 1)
if [ -z "$bin" ]; then
    echo "the ffmpeg archive had no bin/ffmpeg" >&2
    exit 1
fi
install -m 755 "$bin" "$root/packaging/ffmpeg"
echo "wrote $root/packaging/ffmpeg"
