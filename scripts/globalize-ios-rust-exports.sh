#!/bin/sh
set -eu

# Xcode 27 keeps SwiftRs bridge functions local when a SwiftPM package is
# copied into the Rust application archive.  The Rust crate calls these
# functions from a different archive member, so export the bridge symbols in
# the final libapp.a immediately before the native linker runs.
OBJCOPY="$(rustc --print sysroot)/lib/rustlib/aarch64-apple-darwin/bin/llvm-objcopy"
if [ ! -x "$OBJCOPY" ]; then
  echo "iOS Rust archive export fixer: llvm-objcopy is unavailable" >&2
  exit 1
fi

for archive in "$@"; do
  [ -f "$archive" ] || continue
  "$OBJCOPY" \
    --globalize-symbol=_retain_object \
    --globalize-symbol=_release_object \
    --globalize-symbol=_data_from_bytes \
    --globalize-symbol=_string_from_bytes \
    "$archive"
  xcrun ranlib "$archive"
done
