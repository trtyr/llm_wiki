import { useState } from "react"
import { getApiBase, setApiBase, setToken } from "../api/client"

/** First-run gate: collect the API token (and optional API base for
 * remote deployments) before any query fires. */
export function TokenGate({ onReady }: { onReady: () => void }) {
  const [token, setTokenValue] = useState("")
  const [base, setBaseValue] = useState(getApiBase())
  const [error, setError] = useState("")

  return (
    <div className="flex min-h-screen items-center justify-center bg-neutral-100 p-6">
      <form
        className="w-full max-w-md rounded-xl border border-neutral-200 bg-white p-8 shadow-sm"
        onSubmit={(event) => {
          event.preventDefault()
          if (!token.trim()) {
            setError("请输入 API Token")
            return
          }
          setToken(token)
          setApiBase(base)
          onReady()
        }}
      >
        <h1 className="mb-1 text-xl font-semibold text-neutral-900">LLM Wiki</h1>
        <p className="mb-6 text-sm text-neutral-500">
          输入后端 API Token 以连接你的知识库（token 见服务端 scripts/llm-server.config.json）。
        </p>
        <label className="mb-1 block text-sm font-medium text-neutral-700">API Token</label>
        <input
          type="password"
          value={token}
          onChange={(event) => setTokenValue(event.target.value)}
          placeholder="Bearer token"
          className="mb-4 w-full rounded-lg border border-neutral-300 px-3 py-2 text-sm focus:border-blue-500 focus:outline-none"
        />
        <label className="mb-1 block text-sm font-medium text-neutral-700">
          API 地址（可选，默认同源）
        </label>
        <input
          value={base}
          onChange={(event) => setBaseValue(event.target.value)}
          placeholder="http://100.64.0.6:19828"
          className="mb-2 w-full rounded-lg border border-neutral-300 px-3 py-2 text-sm focus:border-blue-500 focus:outline-none"
        />
        {error && <p className="mb-2 text-sm text-red-600">{error}</p>}
        <button
          type="submit"
          className="mt-2 w-full rounded-lg bg-neutral-900 px-4 py-2 text-sm font-medium text-white hover:bg-neutral-700"
        >
          连接
        </button>
      </form>
    </div>
  )
}
