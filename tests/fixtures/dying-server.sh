#!/bin/sh
# Positional stdio MCP "server" for the transport-failure E2E: answers
# initialize and tools/list, then EXITS on the first tools/call — the
# subprocess-died case. min-mcp's client ids are deterministic (initialize=1,
# tools/list=2), so positional replies are enough.
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"dying","version":"0"}}}'
read -r _initialized_notification
read -r _tools_list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"boom","description":"Crashes the server.","inputSchema":{"type":"object","properties":{}}}]}}'
read -r _tools_call
exit 0
