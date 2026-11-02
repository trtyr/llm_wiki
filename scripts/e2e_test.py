#!/usr/bin/env python3
"""End-to-end test suite for the LLM Wiki backend (llm-server).

Runs against a LIVE server and exercises every API surface: discovery,
auth, settings, projects, files CRUD, page write/embed, hybrid search
(including ghost-page checks), history round-trip, rebuild-index,
reviews, graph, sessions, chat (blocking + SSE + cancel), and
concurrency.

Usage:
    python scripts/e2e_test.py --base http://127.0.0.1:19828 --token <TOKEN>

Stdlib only. Creates a dedicated throwaway project for content tests
and removes it at the end.
"""

import argparse
import concurrent.futures
import http.client
import json
import shutil
import sys
import threading
import time
import uuid
from pathlib import Path
from urllib.parse import urlencode

PASS = 0
FAIL = 0
FAILURES = []


SKIP_LLM = False


def check(name, ok, detail=""):
    global SKIP_LLM
    global PASS, FAIL
    if SKIP_LLM and ("blocking chat" in name or "SSE" in name or "cancel endpoint" in name or "long chat" in name):
        print(f"  SKIP  {name} (--skip-llm)")
        return
    if ok:
        PASS += 1
        print(f"  PASS  {name}")
    else:
        FAIL += 1
        FAILURES.append((name, detail))
        print(f"  FAIL  {name}  {detail}")


class Client:
    def __init__(self, base_host, base_port, token):
        self.host = base_host
        self.port = base_port
        self.token = token

    def request(self, method, path, body=None, token=None, headers=None, timeout=120):
        conn = http.client.HTTPConnection(self.host, self.port, timeout=timeout)
        hdrs = dict(headers or {})
        effective_token = token if token is not None else self.token
        if effective_token:
            hdrs["Authorization"] = f"Bearer {effective_token}"
        payload = None
        if body is not None:
            payload = json.dumps(body)
            hdrs["Content-Type"] = "application/json"
        conn.request(method, path, body=payload, headers=hdrs)
        res = conn.getresponse()
        raw = res.read().decode("utf-8", "replace")
        status = res.status
        conn.close()
        try:
            parsed = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            parsed = {"_raw": raw}
        return status, parsed, raw


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:19828")
    ap.add_argument("--token", required=True)
    ap.add_argument("--keep", action="store_true", help="keep the throwaway project dir")
    ap.add_argument("--skip-llm", action="store_true", help="skip checks that call a real LLM (CI: no provider key)")
    args = ap.parse_args()
    if args.skip_llm:
        global SKIP_LLM
        SKIP_LLM = True

    base = args.base.rstrip("/")
    if "://" in base:
        after = base.split("://", 1)[1]
    else:
        after = base
    host, _, port = after.partition(":")
    c = Client(host, int(port or 80), args.token)
    project_root = Path(base.split("://", 1)[0] == "https" and "." or ".").absolute()  # unused; local fs cleanup below
    test_parent = None
    project_dir = None
    print(f"== E2E against {base}")

    # ------------------------------------------------------------------
    print("\n[1] discovery & health")
    status, body, _ = c.request("GET", "/health", token="")
    check("health: no auth, ok", status == 200 and body.get("ok") is True, str(body)[:120])
    status, body, raw = c.request("GET", "/openapi.json", token="")
    check("openapi: served without auth", status == 200 and body.get("openapi", "").startswith("3.1"), str(body)[:80])
    spec_paths = set(body.get("paths", {}).keys())
    check("openapi: 26 paths documented", len(spec_paths) == 26, f"got {len(spec_paths)}")
    status, body, _ = c.request("GET", "/", token="")
    check("root / serves openapi", status == 200 and "openapi" in body, "")
    status, _, raw = c.request("GET", "/docs", token="")
    check("docs page gated (no token -> 401)", status == 401, f"got {status}")
    status, _, raw = c.request("GET", f"/docs?token={args.token}", token="")
    check("docs page renders with query token", status == 200 and "<html" in raw and "curl" in raw and "API 文档" in raw, f"{status} len={len(raw)}")
    status, health, _ = c.request("GET", "/health", token="")
    check("health advertises /docs", health.get("docs", "").startswith("/docs"), str(health.get("docs"))[:60])
    status, body, _ = c.request("GET", "/api/v1/openapi.json")
    check("openapi alias under /api/v1", status == 200 and "paths" in body, "")

    # ------------------------------------------------------------------
    print("\n[2] auth enforcement")
    for method, path in [
        ("GET", "/api/v1/settings"),
        ("PUT", "/api/v1/settings"),
        ("GET", "/api/v1/projects"),
        ("POST", "/api/v1/projects/current/chat"),
    ]:
        status, _, _ = c.request(method, path, token="", body={})
        check(f"no token -> 401 ({method} {path.split('?')[0]})", status == 401, f"got {status}")
    status, _, _ = c.request("GET", "/api/v1/projects", token="wrong-token-xyz")
    check("wrong token -> 401", status == 401, f"got {status}")
    status, _, _ = c.request("DELETE", "/api/v1/settings")
    check("DELETE settings -> 404 (no such route)", status == 404, f"got {status}")
    status, _, _ = c.request("GET", "/api/v1/projects/current/chat")
    check("GET chat endpoint -> 404", status == 404, f"got {status}")

    # ------------------------------------------------------------------
    print("\n[3] settings read/write")
    status, before, _ = c.request("GET", "/api/v1/settings")
    check("settings GET returns object", status == 200 and isinstance(before, dict), str(before)[:80])
    check("settings GET carries llmConfig", isinstance(before.get("llmConfig"), dict), str(before.get("llmConfig"))[:60])
    check("settings GET masks apiKey", before.get("llmConfig", {}).get("apiKey") in (None, "********"), "apiKey must not be returned in clear")
    probe = "e2e-probe-" + uuid.uuid4().hex[:8]
    status, merged, _ = c.request(
        "PUT", "/api/v1/settings",
        body={"e2eProbe": probe, "llmConfig": before.get("llmConfig", {})},
    )
    check("settings PUT merges keys", status == 200 and merged.get("e2eProbe") == probe, str(merged)[:100])
    status, after, _ = c.request("GET", "/api/v1/settings")
    check("settings PUT persisted", after.get("e2eProbe") == probe, "")
    # cleanup probe key (merge with None removal is unsupported; write back without it)
    status, _, _ = c.request("PUT", "/api/v1/settings", body={"e2eProbe": None})
    status, after, _ = c.request("GET", "/api/v1/settings")
    check("settings null-deletion removes probe key", status == 200 and "e2eProbe" not in after, "")

    # ------------------------------------------------------------------
    print("\n[4] projects: list / create / current pinning")
    status, before_projects, _ = c.request("GET", "/api/v1/projects")
    check("projects list ok", status == 200 and "projects" in before_projects, "")
    check("projects list carries currentProject", "currentProject" in before_projects, str(before_projects)[:120])
    default_path = (before_projects.get("currentProject") or {}).get("path", "")
    stamp = uuid.uuid4().hex[:8]
    parent = None
    # find a parent dir on the same drive as existing projects (ask via existing project path)
    if before_projects.get("projects"):
        first_path = before_projects["projects"][0]["path"]  # e.g. D:/llm-wiki-data/default-wiki
        parent = str(Path(first_path).parent)
    if not parent:
        parent = "."
    test_name = f"e2e-test-{stamp}"
    status, created, _ = c.request(
        "POST", "/api/v1/projects/create",
        body={"name": test_name, "path": parent},
    )
    project_dir = None
    if status == 200:
        project_dir = created.get("project", {}).get("path")
    check("projects/create scaffolds + pins current", status == 200 and project_dir, str(created)[:150])
    project_ref = created.get("project", {}).get("id") or test_name
    P = f"/api/v1/projects/{project_ref}"
    status, listed, _ = c.request("GET", "/api/v1/projects")
    names = [p["name"] for p in listed.get("projects", [])]
    check("created project is listed", test_name in names, str(names))
    status, repin, _ = c.request("POST", "/api/v1/projects/current", body={"path": default_path})
    check("re-pin current back works", status == 200 and repin.get("ok") is True, str(repin)[:120])
    status, bad, _ = c.request("POST", "/api/v1/projects/current", body={"path": "Z:/definitely/missing/dir"})
    check("re-pin missing dir -> 404", status == 404, f"got {status}")
    test_scope = test_name  # use the throwaway project by name for content tests

    # ------------------------------------------------------------------
    print("\n[5] files: list / read / write / overwrite semantics / delete")
    status, files, _ = c.request("GET", f"{P}/files?root=wiki")
    check("files list ok (fresh scaffold)", status == 200 and files.get("files") is not None, "")
    demo = "wiki/concepts/e2e-demo.md"
    status, wr, _ = c.request("POST", f"{P}/pages/write", body={"path": demo, "content": "# v1\n\ne2e-demo-token-42"})
    check("pages/write creates", status == 200 and wr.get("result", {}).get("verified") is True, str(wr)[:120])
    status, wr2, _ = c.request("POST", f"{P}/pages/write", body={"path": demo, "content": "# v2"})
    check("pages/write without allowOverwrite -> 409", status == 409, f"got {status}")
    status, wr3, _ = c.request("POST", f"{P}/pages/write", body={"path": demo, "content": "# v2\n\noverwritten", "allowOverwrite": True})
    check("pages/write allowOverwrite -> 200", status == 200, f"got {status}")
    status, content, _ = c.request("GET", f"{P}/files/content?path={demo}")
    check("files/content reads back v2", status == 200 and content.get("content", "").startswith("# v2"), str(content)[:100])
    status, body, _ = c.request("DELETE", f"{P}/files/content?path={demo}")
    check("DELETE removes file", status == 200 and body.get("ok") is True, str(body)[:100])
    status, _, _ = c.request("GET", f"{P}/files/content?path={demo}")
    check("deleted file reads 404", status == 404, f"got {status}")
    status, guard, _ = c.request("DELETE", f"{P}/files/content?path=.llm-wiki/project.json")
    check("delete guard: dot-path -> 403", status == 403, f"got {status}")

    # ------------------------------------------------------------------
    print("\n[6] history round-trip")
    c.request("PUT", f"{P}/history/settings", body={"enabled": True, "maxVersionsPerFile": 10})
    c.request("POST", f"{P}/pages/write", body={"path": demo, "content": "# v1\n\ne2e-demo-token-42"})
    c.request("POST", f"{P}/pages/write", body={"path": demo, "content": "# v2\n\noverwritten", "allowOverwrite": True})
    status, hist, _ = c.request("GET", f"{P}/history?path={demo}")
    entries = hist.get("entries", [])
    check("history lists versions", status == 200 and len(entries) >= 1, f"entries={len(entries)}")
    if entries:
        oldest = entries[-1]["id"]
        status, res, _ = c.request("POST", f"{P}/history/restore", body={"path": demo, "entryId": oldest})
        check("history restore ok", status == 200 and res.get("ok") is True, str(res)[:120])
        status, content, _ = c.request("GET", f"{P}/files/content?path={demo}")
        check("restored content is oldest version", content.get("content", "").startswith("# v1"), str(content)[:60])
    status, stats, _ = c.request("GET", f"{P}/history/stats")
    check("history stats ok", status == 200 and stats.get("ok") is True, str(stats)[:100])
    status, hs, _ = c.request("GET", f"{P}/history/settings")
    check("history settings GET ok", status == 200 and hs.get("settings") is not None, "")
    status, hs2, _ = c.request("PUT", f"{P}/history/settings", body={"enabled": False, "maxVersionsPerFile": 10})
    check("history settings PUT ok", status == 200 and hs2.get("settings", {}).get("enabled") is False, "")
    c.request("DELETE", f"{P}/files/content?path={demo}")

    # ------------------------------------------------------------------
    print("\n[7] search / embed / graph / rebuild-index / rescan")
    seed_page = "wiki/concepts/e2e-search-seed.md"
    token_phrase = f"quantum-umbrella-{uuid.uuid4().hex[:6]}"
    c.request("PUT", f"{P}/history/settings", body={"enabled": True, "maxVersionsPerFile": 10})
    c.request("POST", f"{P}/pages/write", body={"path": seed_page, "content": f"# Seed\n\nunique marker {token_phrase} for search verification."})
    status, search, _ = c.request("POST", f"{P}/search", body={"query": token_phrase, "topK": 5, "includeContent": True})
    check("keyword search finds seeded page", status == 200 and len(search.get("results", [])) >= 1, str(search)[:120])
    status, emb, _ = c.request("POST", f"{P}/pages/embed", body={"path": seed_page})
    embedded = emb.get("result", {}).get("vectorsWritten", 0)
    check("pages/embed writes vectors", status == 200 and embedded >= 1, str(emb)[:120])
    status, search2, _ = c.request("POST", f"{P}/search", body={"query": token_phrase, "topK": 5})
    check("search after embed ok", status == 200, "")
    status, graph, _ = c.request("GET", f"{P}/graph")
    check("graph returns nodes", status == 200 and "nodes" in str(graph)[:2000], "")
    status, rebuild, _ = c.request("POST", f"{P}/maintenance/rebuild-index")
    check("rebuild-index ok", status == 200 and rebuild.get("ok") is True, str(rebuild)[:120])
    status, rescan, _ = c.request("POST", f"{P}/sources/rescan")
    check("sources/rescan ok", status == 200 and rescan.get("ok") is True, str(rescan)[:120])

    # ghost check: delete the page, ensure search no longer returns it
    c.request("DELETE", f"{P}/files/content?path={seed_page}")
    status, search3, _ = c.request("POST", f"{P}/search", body={"query": token_phrase, "topK": 10, "includeContent": True})
    ghost = [r for r in search3.get("results", []) if "e2e-search-seed" in (r.get("path") or "")]
    check("no ghost results after delete", status == 200 and not ghost, str(ghost)[:150])

    # ------------------------------------------------------------------
    print("\n[8] reviews")
    status, reviews, _ = c.request("GET", f"{P}/reviews")
    check("reviews list ok", status == 200 and "reviews" in str(reviews)[:200], str(reviews)[:100])

    # ------------------------------------------------------------------
    print("\n[9] chat: blocking / SSE / cancel / sessions")
    status, chat, _ = c.request("POST", f"{P}/chat", body={"message": "只回答两个字：在线", "persistSession": True, "sessionId": f"e2e-{stamp}"}, timeout=180)
    reply = (chat.get("message") or {}).get("content", "") if isinstance(chat, dict) else ""
    check("blocking chat replies", status == 200 and chat.get("ok") is True and len(reply) > 0, str(chat)[:150])
    session_id = chat.get("sessionId")
    check("chat returns sessionId", bool(session_id), "")
    status, sessions, _ = c.request("GET", f"{P}/chat/sessions")
    ids = [s.get("sessionId") for s in sessions.get("sessions", [])]
    check("session listed", session_id in ids, str(ids)[:100])
    status, history, _ = c.request("GET", f"{P}/chat/sessions/{session_id}?limit=10")
    check("session history has both turns", status == 200 and len(history.get("messages", [])) >= 2, str(history)[:120])

    # SSE streaming
    conn = http.client.HTTPConnection(c.host, c.port, timeout=180)
    conn.request(
        "POST", f"{P}/chat",
        body=json.dumps({"message": "用一句话介绍这个项目", "persistSession": False, "stream": True}),
        headers={"Authorization": f"Bearer {c.token}", "Content-Type": "application/json",
                 "Accept": "text/event-stream"},
    )
    res = conn.getresponse()
    ctype = res.getheader("Content-Type", "")
    frames = []
    deadline = time.time() + 150
    buf = b""
    while time.time() < deadline:
        chunk = res.read1(4096) if hasattr(res, "read1") else res.read(100)
        if not chunk:
            break
        buf += chunk
        text = buf.decode("utf-8", "replace")
        if "event: done" in text or "event: error" in text:
            break
    conn.close()
    sse_text = buf.decode("utf-8", "replace")
    check("SSE: content-type is event-stream", "text/event-stream" in ctype, ctype)
    check("SSE: meta frame present", "event: meta" in sse_text, sse_text[:120])
    check("SSE: agent events present", "event: agent" in sse_text, "")
    check("SSE: done frame present", "event: done" in sse_text, sse_text[-200:])

    # cancel
    cancel_session = f"e2e-cancel-{stamp}"
    try:
        c.request(
            "POST", f"{P}/chat",
            body={"message": "请写一篇800字的关于星空的散文，不要停", "persistSession": False, "sessionId": cancel_session},
            timeout=5,
        )
        check("long chat still running after 5s", False, "finished too fast to cancel")
    except TimeoutError:
        check("long chat still running after 5s", True, "")
    except OSError as err:
        check("long chat still running after 5s", False, f"connection failed: {err}")
    time.sleep(1.5)
    status, cancel_res, _ = c.request("POST", f"{P}/chat/{cancel_session}/cancel")
    check("cancel endpoint responds", status == 200 and cancel_res.get("cancelled") is True, str(cancel_res)[:100])

    # ------------------------------------------------------------------
    print("\n[10] concurrency")
    def one_search(i):
        return c.request("POST", f"{P}/search", body={"query": f"q{i}", "topK": 1})[0]
    with concurrent.futures.ThreadPoolExecutor(max_workers=5) as pool:
        codes = list(pool.map(one_search, range(5)))
    check("5 concurrent searches all 200", all(code == 200 for code in codes), str(codes))

    # ------------------------------------------------------------------
    print("\n[11] cleanup")
    c.request("POST", "/api/v1/projects/current", body={"path": default_path or ""})
    if project_dir and not args.keep:
        try:
            shutil.rmtree(project_dir)
            check("throwaway project dir removed", not Path(project_dir).exists(), project_dir)
        except OSError as err:
            check("throwaway project dir removed", False, str(err))
    elif project_dir:
        print(f"  (kept) {project_dir}")

    # ------------------------------------------------------------------
    print(f"\n{'=' * 60}\nRESULT: {PASS} passed, {FAIL} failed")
    if FAILURES:
        print("\nFailed checks:")
        for name, detail in FAILURES:
            print(f"  - {name}: {detail[:160]}")
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
