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


def check_set_runner(rpc):
    """session.set_runner switches the active runner for a session (switchover)."""
    sid = rpc.call("session.create", {"title": "smoke-set-runner"})["id"]
    res = rpc.call("session.set_runner", {"session_id": sid, "runner": "copilot"})
    if res.get("runner") != "copilot":
        print(f"FAIL [set_runner]: expected runner 'copilot', got {res.get('runner')!r}")
        return False
    print(f"PASS [set_runner]: session {sid[:8]} runner -> {res['runner']!r}")
    return True


def check_takeover(rpc):
    """session.takeover atomically forks a session onto a new runner."""
    sid = rpc.call("session.create", {"title": "smoke-takeover"})["id"]
    res = rpc.call("session.takeover", {"session_id": sid, "runner": "copilot"})
    new_sid = res.get("new_session_id")
    if not new_sid or new_sid == sid:
        print(f"FAIL [takeover]: expected a new forked session id, got {new_sid!r}")
        return False
    if res.get("runner") != "copilot":
        print(f"FAIL [takeover]: expected runner 'copilot', got {res.get('runner')!r}")
        return False
    print(f"PASS [takeover]: {sid[:8]} -> forked {new_sid[:8]} on {res['runner']!r}")
    return True


def check_rollback(rpc):
    """Two turns create checkpoints at turn_n 0 and 1; rollback to 0 must succeed."""
    sid = rpc.call("session.create", {"title": "smoke-rollback"})["id"]
    for content in ["Reply with one word: one", "Reply with one word: two"]:
        res = turn(rpc, sid, content)
        if res.get("error"):
            print(f"FAIL [rollback/setup]: turn errored: {res.get('error')}")
            return False
    res = rpc.call("session.rollback", {"session_id": sid, "turn_n": 0})
    if res.get("turn_n") != 0 or "messages_json" not in res:
        print(f"FAIL [rollback]: unexpected result {json.dumps(res)[:200]}")
        return False
    print(f"PASS [rollback]: session {sid[:8]} rolled back to turn_n=0")
    return True


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

    # 3) Session control: switchover, takeover, and rollback.
    if not check_set_runner(rpc):
        return 1
    if not check_takeover(rpc):
        return 1
    if not check_rollback(rpc):
        return 1

    print("PASS: prompting (single+multi-turn), set_runner, takeover, and rollback all work")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:  # noqa: BLE001
        print(f"FAIL: {e}")
        sys.exit(2)
