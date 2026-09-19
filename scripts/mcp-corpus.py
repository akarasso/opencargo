#!/usr/bin/env python3
"""Rebuild tests/fixtures/mcp/descriptions.json: the tools/list answers of
the reachable remotes of the committed live page, plus the hand-written
benign set, with the number of servers that answered in the header."""

import json
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PAGE = ROOT / "tests/fixtures/mcp/live-page.json"
BENIGN = ROOT / "tests/fixtures/mcp/benign.json"
OUT = ROOT / "tests/fixtures/mcp/descriptions.json"
TIMEOUT = 15


def post(url, body, headers):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST")
    req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "application/json, text/event-stream")
    for k, v in headers.items():
        req.add_header(k, v)
    with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
        raw = resp.read(4 << 20).decode("utf-8", "replace")
        session = resp.headers.get("Mcp-Session-Id")
    if raw.lstrip().startswith("{"):
        return json.loads(raw), session
    for line in raw.splitlines():
        if line.startswith("data:"):
            try:
                msg = json.loads(line[5:].strip())
            except ValueError:
                continue
            if msg.get("id") == body.get("id"):
                return msg, session
    return None, session


def modern(url):
    meta = {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "opencargo-corpus", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    }
    body = {"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": meta}}
    msg, _ = post(url, body, {"MCP-Protocol-Version": "2026-07-28", "Mcp-Method": "tools/list"})
    return msg


def legacy(url):
    init = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "opencargo-corpus", "version": "1"}}}
    msg, session = post(url, init, {})
    version = (msg or {}).get("result", {}).get("protocolVersion", "2025-06-18")
    headers = {"MCP-Protocol-Version": version}
    if session:
        headers["Mcp-Session-Id"] = session
    try:
        post(url, {"jsonrpc": "2.0", "method": "notifications/initialized"}, headers)
    except Exception:
        pass
    msg, _ = post(url, {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}, headers)
    return msg


def tools_of(url):
    for attempt in (modern, legacy):
        try:
            msg = attempt(url)
        except Exception:
            continue
        tools = (msg or {}).get("result", {}).get("tools")
        if isinstance(tools, list):
            return tools
    return None


def main():
    page = json.loads(PAGE.read_text())
    entries, servers, seen = [], 0, set()
    for envelope in page["servers"]:
        server = envelope["server"]
        if server["name"] in seen:
            continue
        seen.add(server["name"])
        for remote in server.get("remotes", []):
            if remote.get("type") != "streamable-http" or "{" in remote.get("url", ""):
                continue
            tools = tools_of(remote["url"])
            if tools is None:
                continue
            servers += 1
            for t in tools:
                entries.append({"server": server["name"], "tool": t})
            print(f"{server['name']}: {len(tools)} tools", file=sys.stderr)
            break
    benign = json.loads(BENIGN.read_text())
    OUT.write_text(json.dumps({"probed_servers": servers, "tools": entries, "benign": benign}, indent=1, ensure_ascii=False) + "\n")
    print(f"{servers} servers answered, {len(entries)} tools", file=sys.stderr)


if __name__ == "__main__":
    main()
