#!/bin/sh
# Minimal positional stdio MCP "server" for the timeout E2E: answers
# initialize and tools/list, then never answers a tools/call. Positional
# because min-mcp's client ids are deterministic (initialize=1, tools/list=2).
read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"slow","version":"0"}}}'
read -r _initialized_notification
read -r _tools_list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"sleepy","description":"Waits forever.","inputSchema":{"type":"object","properties":{}}}]}}'
# every subsequent request (tools/call) gets silence
while read -r _line; do :; done
