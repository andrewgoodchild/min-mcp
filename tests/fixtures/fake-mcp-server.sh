#!/bin/sh
# A tiny scriptable stdio MCP server for offline integration tests.
#
# Unlike slow-server.sh (which never answers a call), this one dispatches on the
# request METHOD and tool NAME, so tests can exercise features that need a real
# upstream that actually returns data: composites (`workflows:`), `minmcp verify`,
# and auto-pagination. Line-driven rather than positional, so extra requests
# (ping, a second tools/list) don't desynchronise it.
#
# Tools it serves:
#   create_product         -> {"id":"prod_1","name":<name>}
#   create_price           -> {"id":"price_1","product":<product>}
#   list_items             -> paginated: page 1 has next_cursor, page 2 ends it
#   whoami                 -> {"user":"tester","plan":"pro"}   (for verify checks)
#
# POSIX sh only: no bashisms, no jq. Crude string matching is deliberate — the
# inputs are this file's own tests.

emit() { printf '%s\n' "$1"; }

while IFS= read -r line; do
  # request id (integer; these tests never send string ids)
  id=$(printf '%s' "$line" | sed -n 's/.*"id":[[:space:]]*\([0-9]*\).*/\1/p')
  [ -n "$id" ] || id=0

  case "$line" in
    *'"method":"initialize"'*)
      emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}'
      ;;

    *'"method":"notifications/initialized"'*)
      : # notification: no reply
      ;;

    *'"method":"ping"'*)
      emit '{"jsonrpc":"2.0","id":'"$id"',"result":{}}'
      ;;

    *'"method":"tools/list"'*)
      # one line: a JSON-RPC frame is newline-delimited
      emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"tools":[{"name":"create_product","description":"Create a product.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}},{"name":"create_price","description":"Create a price for a product.","inputSchema":{"type":"object","properties":{"product":{"type":"string"},"amount":{"type":"integer"}},"required":["product","amount"]}},{"name":"list_items","description":"List items, paginated.","inputSchema":{"type":"object","properties":{"cursor":{"type":"string"}}},"annotations":{"readOnlyHint":true}},{"name":"whoami","description":"Return the current user.","inputSchema":{"type":"object","properties":{}},"annotations":{"readOnlyHint":true}}]}}'
      ;;

    *'"method":"tools/call"'*)
      case "$line" in
        *'"name":"create_product"'*)
          emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"{\"id\":\"prod_1\",\"object\":\"product\"}"}],"isError":false}}'
          ;;
        *'"name":"create_price"'*)
          # echo back which product it was threaded with, so a composite can be
          # proven to have passed step 1's output into step 2
          case "$line" in
            *'prod_1'*) emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"{\"id\":\"price_1\",\"product\":\"prod_1\"}"}],"isError":false}}' ;;
            *)          emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"{\"error\":\"product not threaded\"}"}],"isError":true}}' ;;
          esac
          ;;
        *'"name":"list_items"'*)
          # page 2 when a cursor was supplied, else page 1 + a cursor
          case "$line" in
            *'"cursor":"c2"'*)
              emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"{\"data\":[{\"id\":\"i3\"},{\"id\":\"i4\"}],\"has_more\":false,\"next_cursor\":null}"}],"isError":false}}'
              ;;
            *)
              emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"{\"data\":[{\"id\":\"i1\"},{\"id\":\"i2\"}],\"has_more\":true,\"next_cursor\":\"c2\"}"}],"isError":false}}'
              ;;
          esac
          ;;
        *'"name":"whoami"'*)
          emit '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"{\"user\":\"tester\",\"plan\":\"pro\",\"secret\":\"shh\"}"}],"isError":false}}'
          ;;
        *)
          emit '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32601,"message":"unknown tool"}}'
          ;;
      esac
      ;;

    *) : ;;  # anything else: ignore
  esac
done
