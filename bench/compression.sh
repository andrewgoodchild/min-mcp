#!/usr/bin/env bash
# Reproduce the token-compression claim — deterministic, no network, no LLM, no
# credentials. min-mcp reports the naive one-tool-per-endpoint token cost of a
# spec (`est_tokens_raw`) vs the 3-tool minified surface (`est_tokens_minified`);
# the ratio is the compression. The two bundled specs run with just this repo.
#
#   ./bench/compression.sh
#
# To reproduce the headline number on a real API, download its OpenAPI spec next
# to examples/stripe-from-spec.yaml (as ./stripe.json) and uncomment the line below.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${MINMCP:-./target/release/minmcp}
[ -x "$BIN" ] || { echo "build the binary first:  cargo build --release" >&2; exit 1; }

measure() {  # <label> <config>
  "$BIN" inspect --config "$2" | python3 -c '
import json,sys
d=json.load(sys.stdin); label=sys.argv[1]
raw,mini = d["est_tokens_raw"], d["est_tokens_minified"]
ut,st = d["upstream_tools"], d["surface_tools"]
print(f"| {label} | {ut} → {st} | {raw:,} | {mini:,} | {raw/mini:.0f}× |")
' "$1"
}

echo "| spec | tools | raw tokens | minified | compression |"
echo "|---|---|---:|---:|---:|"
measure "acme-store (bundled, 4 ops)"  examples/demo-overlays.yaml
measure "bigapi (bundled, 120 ops)"    bench/bigapi.yaml
# measure "stripe (587 ops)"           examples/stripe-from-spec.yaml   # needs ./stripe.json
echo
echo "Minified tokens stay ~flat as the surface grows; raw scales with tool count."
