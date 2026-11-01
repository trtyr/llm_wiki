#!/usr/bin/env node
// Streamable HTTP entrypoint: exposes the same LLM Wiki MCP tools over the
// official Streamable HTTP transport, so MCP clients can connect to a
// running instance as a remote server instead of spawning a stdio child.
//
//   MCP_HTTP_HOST=100.64.0.6 MCP_HTTP_PORT=8931 \
//   LLM_WIKI_API_TOKEN=... node dist/src/http.js
//
// Design notes:
// - Stateless mode (sessionIdGenerator: undefined): every request stands
//   alone, no session lifecycle to leak or break — the right shape for a
//   single-user, tool-call-heavy personal endpoint. Following the official
//   SDK stateless pattern, each request gets a fresh server + transport
//   pair, closed when the response completes.
// - enableJsonResponse: plain JSON replies instead of SSE streams; clients
//   that negotiate streaming still work through the SDK.
// - Token auth on every request (MCP_HTTP_TOKEN, falling back to
//   LLM_WIKI_API_TOKEN) plus DNS-rebinding protection.
// - llm_wiki_set_project binds per process, so it is shared by all HTTP
//   clients of this instance — fine for a personal server, not multi-tenant.

import http from "node:http"
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js"
import { createWikiServer } from "./index.js"

const HOST = process.env.MCP_HTTP_HOST ?? "127.0.0.1"
const PORT = Number(process.env.MCP_HTTP_PORT ?? 8931)
const TOKEN =
  process.env.MCP_HTTP_TOKEN ?? process.env.LLM_WIKI_API_TOKEN ?? ""
if (!TOKEN.trim()) {
  console.error(
    "[llm-wiki-http] no token configured; set MCP_HTTP_TOKEN or LLM_WIKI_API_TOKEN",
  )
  process.exit(1)
}

const allowedHosts = (
  process.env.MCP_HTTP_ALLOWED_HOSTS ??
  [`${HOST}:${PORT}`, `127.0.0.1:${PORT}`, `localhost:${PORT}`, `[::1]:${PORT}`]
    .join(",")
).split(",").map((h) => h.trim()).filter(Boolean)

function tokenOk(req: http.IncomingMessage): boolean {
  const header = req.headers.authorization ?? ""
  const expected = `Bearer ${TOKEN}`
  if (header.length !== expected.length) return false
  let diff = 0
  for (let i = 0; i < expected.length; i++) {
    diff |= header.charCodeAt(i) ^ expected.charCodeAt(i)
  }
  return diff === 0
}

function respondJson(res: http.ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "Content-Type": "application/json" })
  res.end(JSON.stringify(body))
}

async function readJsonBody(req: http.IncomingMessage): Promise<unknown> {
  let raw = ""
  for await (const chunk of req) raw += chunk
  if (!raw.trim()) return undefined
  return JSON.parse(raw)
}

const httpServer = http.createServer((req, res) => {
  void (async () => {
    if (!tokenOk(req)) {
      return respondJson(res, 401, {
        jsonrpc: "2.0",
        error: { code: -32001, message: "unauthorized" },
      })
    }
    if (req.method !== "POST") {
      // Stateless servers expose no request stream (GET) and no session to
      // terminate (DELETE); the spec allows 405 for both.
      return respondJson(res, 405, {
        jsonrpc: "2.0",
        error: { code: -32000, message: "stateless server: POST only" },
      })
    }

    let body: unknown
    try {
      body = await readJsonBody(req)
    } catch {
      return respondJson(res, 400, {
        jsonrpc: "2.0",
        error: { code: -32700, message: "parse error" },
      })
    }

    // Fresh server + transport per request (official stateless pattern).
    const instance = createWikiServer()
    const transport = new StreamableHTTPServerTransport({
      sessionIdGenerator: undefined,
      enableJsonResponse: true,
      enableDnsRebindingProtection: true,
      allowedHosts,
    })
    res.on("close", () => {
      void transport.close()
      void instance.close()
    })
    await instance.connect(transport)
    await transport.handleRequest(req, res, body)
  })().catch((err) => {
    console.error("[llm-wiki-http] request handling failed:", err)
    if (!res.headersSent) {
      respondJson(res, 500, {
        jsonrpc: "2.0",
        error: { code: -32603, message: "internal error" },
      })
    } else {
      res.end()
    }
  })
})

httpServer.listen(PORT, HOST, () => {
  console.error(
    `[llm-wiki-http] MCP Streamable HTTP on http://${HOST}:${PORT}/ (stateless, Bearer auth, allowed hosts: ${allowedHosts.join(", ")})`,
  )
})

function shutdown(): void {
  console.error("[llm-wiki-http] shutting down")
  httpServer.close()
}
process.on("SIGINT", shutdown)
process.on("SIGTERM", shutdown)
