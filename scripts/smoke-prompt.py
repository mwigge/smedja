#!/usr/bin/env python3
"""End-to-end prompt smoke test for smedja.

Drives the live `smdjad` over its newline-delimited JSON-RPC UDS socket:
  session.create -> turn.submit -> turn.subscribe
and asserts the turn reaches a successful terminal state (i.e. the routed
provider actually answered). This is the minimum "can we prompt?" check.

Usage: python3 scripts/smoke-prompt.py ["prompt text"]
Exit 0 on success, non-zero on any failure.
"""
import json
import os
import socket
import sys

SOCK = os.environ.get("SMEDJA_SOCK") or f"/run/user/{os.getuid()}/smdjad.sock"
PROMPT = sys.argv[1] if len(sys.argv) > 1 else "Reply with exactly one word: hello"


class Rpc:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.settimeout(90)
        self.s.connect(path)
        self.buf = b""
        self.id = 0

    def call(self, method, params):
        self.id += 1
        req = json.dumps({"jsonrpc": "2.0", "id": self.id,
                          "method": method, "params": params}) + "\n"
        self.s.sendall(req.encode())
        while b"\n" not in self.buf:
            chunk = self.s.recv(65536)
            if not chunk:
                raise RuntimeError(f"{method}: connection closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        resp = json.loads(line)
        if "error" in resp and resp["error"]:
            raise RuntimeError(f"{method}: RPC error {resp['error']}")
        return resp.get("result")


def turn(rpc, sid, content):
    """Submits one turn and returns the terminal result dict."""
    task_id = rpc.call("turn.submit", {"session_id": sid, "content": content})["task_id"]
    return rpc.call("turn.subscribe", {"task_id": task_id}) or {}


def check_ok(result, label):
    """Asserts a terminal result is a successful, non-empty answer."""
    status = result.get("status", "")
    err = result.get("error") or result.get("error_kind")
    response = (result.get("response") or "").strip()
    if err or status in ("failed", "error"):
        print(f"FAIL [{label}]: turn ended in error (status={status!r} error={err!r})")
        return None
    if not response:
        print(f"FAIL [{label}]: empty assistant response")
        return None
    print(f"PASS [{label}]: {response!r}")
    return response


def main():
    print(f"socket: {SOCK}")
    rpc = Rpc(SOCK)

    # 1) Single-turn: a prompt returns a non-empty answer.
    sid = rpc.call("session.create", {"title": "smoke-prompt"})["id"]
    print(f"session: {sid}")
    if check_ok(turn(rpc, sid, PROMPT), "single-turn") is None:
        return 1

    # 2) Multi-turn: the second turn must recall a fact from the first.
    sid2 = rpc.call("session.create", {"title": "smoke-prompt-multiturn"})["id"]
    if check_ok(turn(rpc, sid2, "My favorite number is 42. Just reply: OK"),
                "multi-turn/1") is None:
        return 1
    recall = check_ok(turn(rpc, sid2, "What is my favorite number? Reply with only the number."),
                      "multi-turn/2")
    if recall is None:
        return 1
    if "42" not in recall:
        print(f"FAIL [multi-turn]: prior context not recalled (got {recall!r}, expected '42')")
        return 1
    print("PASS: single-turn and multi-turn prompting both work")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:  # noqa: BLE001
        print(f"FAIL: {e}")
        sys.exit(2)
