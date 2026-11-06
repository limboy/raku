#!/bin/sh
while IFS= read -r line; do
    printf '%s\n' "$line" >> requests.jsonl
    case "$line" in
        *'"type":"user"'*)
            printf '%s\n' '{"type":"result","is_error":false}'
            ;;
        *'"subtype":"generate_session_title"'*)
            printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"raku-session-title","response":{"title":"修复会话标题"}}}'
            ;;
    esac
done
