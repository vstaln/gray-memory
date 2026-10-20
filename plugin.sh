#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-memory.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-memory" ;;
    *) rel="x86_64-apple-darwin/gray-memory" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-memory" ;;
    *) rel="x86_64-unknown-linux-musl/gray-memory" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-memory.exe" "$here/target/release/gray-memory" "$here/target/debug/gray-memory.exe" "$here/target/debug/gray-memory"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-memory binary not found in $here" >&2
exit 1
