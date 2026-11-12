#!/bin/sh
# Record what the driver actually writes, without model calls or credentials.
fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
while IFS= read -r request; do
    id=$(printf '%s' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    case "$request" in
        *'"method":"initialized"'*) ;;
        *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id" ;;
        *'"method":"thread/start"'*|*'"method":"thread/resume"'*)
            printf '%s\n' "$request" > "$fixture_dir/open.request"
            printf '{"id":%s,"result":{"model":"fixture-model","thread":{"id":"thread-context","turns":[]}}}\n' "$id"
            ;;
        *'"method":"turn/start"'*)
            printf '%s\n' "$request" > "$fixture_dir/prompt.request"
            printf '{"id":%s,"result":{"turn":{"id":"turn-context"}}}\n' "$id"
            printf '{"method":"turn/completed","params":{"threadId":"thread-context","turn":{"id":"turn-context","status":"completed"}}}\n'
            ;;
        *'"method":"mcpServerStatus/list"'*) printf '{"id":%s,"result":{"data":[],"nextCursor":null}}\n' "$id" ;;
        *) printf '{"id":%s,"error":{"code":-32601,"message":"method not found"}}\n' "$id" ;;
    esac
done
