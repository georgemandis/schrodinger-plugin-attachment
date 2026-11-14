#!/bin/sh
# Build the plugin and stage it for install: dist/attachment/{plugin.json,lib/}.
set -eu
cd "$(dirname "$0")/.."

cargo build --release

OUT=dist/attachment
LIB=libschrodinger_plugin_attachment.dylib
rm -rf "$OUT"
mkdir -p "$OUT/lib"
cp plugin.json "$OUT/"
cp "target/release/$LIB" "$OUT/lib/"
# Ad hoc unless CODESIGN_IDENTITY names a certificate.
codesign --force --sign "${CODESIGN_IDENTITY:--}" "$OUT/lib/$LIB"
echo "Staged in $OUT"
