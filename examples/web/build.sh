#!/bin/sh
# Build the web example and lay out a directory that can be served.
#
# Four files end up in dist/: the module, the page, its glue, and the shim. The
# shim is not vendored in this repository — it is copied out of whichever
# `webbluetooth-wasm` the lock file resolved to, so it cannot drift from the ABI
# the module was just compiled against.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH= cd -- "$here/../.." && pwd)
dist="$here/dist"

# No feature flags: the TUI dependencies are gated by target, so the default
# feature set is already correct for wasm. See `Cargo.toml`.
cargo build --manifest-path "$root/Cargo.toml" --example web \
  --target wasm32-unknown-unknown --release

# Ask cargo where the shim is rather than guessing at a registry path, which
# encodes both the version and the checksum directory.
shim=$(
  cargo metadata --manifest-path "$root/Cargo.toml" --format-version 1 \
    --filter-platform wasm32-unknown-unknown |
    python3 -c 'import json, os, sys
packages = json.load(sys.stdin)["packages"]
backend = next(p for p in packages if p["name"] == "webbluetooth-wasm")
print(os.path.join(os.path.dirname(backend["manifest_path"]), "js", "webbluetooth.js"))'
)

mkdir -p "$dist"
cp "$root/target/wasm32-unknown-unknown/release/examples/web.wasm" "$dist/web.wasm"
cp "$shim" "$dist/webbluetooth.js"
cp "$here/index.html" "$here/app.js" "$dist/"

cat <<EOF

Built $dist:
$(cd "$dist" && ls -1 | sed 's/^/  /')

Web Bluetooth needs a secure context, so open it over localhost rather than as
a file:// URL:

  python3 -m http.server --directory $dist 8000

then visit http://localhost:8000/ in Chrome or Edge.
EOF
