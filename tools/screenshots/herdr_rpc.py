r"""herdr_rpc.py <socket_path> <method> -- one JSON-RPC call over herdr's
socket (Windows named pipe: \\.\pipe\<socket_path>). Params JSON is read from
stdin (PS 5.1 mangles quoted JSON argv and prepends a BOM when piping).
Prints the JSON response line."""
import json
import sys
import uuid

sock, method = sys.argv[1], sys.argv[2]
raw = sys.stdin.read().lstrip("﻿").strip() or "{}"
req = {"id": "rpc-" + uuid.uuid4().hex[:8], "method": method, "params": json.loads(raw)}
with open("\\\\.\\pipe\\" + sock, "r+b", buffering=0) as f:
    f.write((json.dumps(req) + "\n").encode("utf-8"))
    out = b""
    while not out.endswith(b"\n"):
        chunk = f.read(65536)
        if not chunk:
            break
        out += chunk
print(out.decode("utf-8").strip())
