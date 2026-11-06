#!/usr/bin/env python3
"""A local Pi/Oh My Pi history RPC fixture; never starts a model turn."""

import json
from pathlib import Path
import sys

session_file = Path.cwd() / "source.jsonl"
with open("history-rpc.log", "a") as log:
    log.write(json.dumps({"args": sys.argv[1:]}) + "\n")
    log.flush()
    for line in sys.stdin:
        request = json.loads(line)
        command = request["type"]
        log.write(json.dumps(request) + "\n")
        log.flush()
        data = {}
        if command == "switch_session":
            session_file = Path(request["sessionPath"])
        elif command in ("fork", "branch", "clone"):
            session_file = Path.cwd() / "forked.jsonl"
        elif command in ("get_fork_messages", "get_branch_messages"):
            data = {"messages": [{"entryId": "turn-1"}, {"entryId": "turn-2"}]}
        elif command == "get_state":
            data = {
                "sessionId": session_file.stem,
                "sessionFile": str(session_file),
                "sessionName": "History fixture",
            }
        print(json.dumps({
            "type": "response",
            "id": request["id"],
            "command": command,
            "success": True,
            "data": data,
        }), flush=True)
