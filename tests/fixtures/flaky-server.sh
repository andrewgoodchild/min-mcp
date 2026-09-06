#!/bin/sh
# Line-driven stdio MCP "server" for the respawn E2E. In its FIRST life it dies
# on the first tools/call (after touching a marker file); once respawned by
# min-mcp it finds the marker and answers normally. The marker path comes from
# MINMCP_FLAKY_MARK so every test run starts from a clean first life.
MARK="${MINMCP_FLAKY_MARK:?set MINMCP_FLAKY_MARK}"
emit() { printf '%s\n' "$1"; }
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":[[:space:]]*\([0-9]*\).*/\1/p')
  [ -n "$id" ] || id=0
  case "$line" in
    *'"method":"initialize"'*)
      emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"flaky","version":"0"}}}' ;;
    *'"method":"notifications/initialized"'*) : ;;
    *'"method":"ping"'*) emit '{"jsonrpc":"2.0","id":'"$id"',"result":{}}' ;;
    *'"method":"tools/list"'*)
      emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"tools":[{"name":"flip","description":"Flips a coin.","inputSchema":{"type":"object","properties":{}}}]}}' ;;
    *'"method":"tools/call"'*)
      if [ ! -e "$MARK" ]; then : > "$MARK"; exit 0; fi
      emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"{\"ok\":true,\"life\":2}"}],"isError":false}}' ;;
    *) : ;;
  esac
done
