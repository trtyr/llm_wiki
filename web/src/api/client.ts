// Thin REST client for the llm-server API. Token lives in localStorage;
// API base defaults to same-origin (dev proxy / prod static hosting) and
// can be overridden for remote deployments.

export class UnauthorizedError extends Error {
  constructor() {
    super("unauthorized")
    this.name = "UnauthorizedError"
  }
}

const TOKEN_KEY = "llmwiki.token"
const BASE_KEY = "llmwiki.apiBase"

export function getToken(): string {
  return localStorage.getItem(TOKEN_KEY) ?? ""
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token.trim())
}

export function getApiBase(): string {
  // Stored override wins; otherwise same-origin (empty string).
  return localStorage.getItem(BASE_KEY) ?? ""
}

export function setApiBase(base: string): void {
  const trimmed = base.trim().replace(/\/$/, "")
  if (trimmed) localStorage.setItem(BASE_KEY, trimmed)
  else localStorage.removeItem(BASE_KEY)
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const token = getToken()
  if (!token) throw new UnauthorizedError()
  let res: Response
  try {
    res = await fetch(getApiBase() + path, {
      ...init,
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
        ...(init?.headers ?? {}),
      },
    })
  } catch (err) {
    throw new Error(`无法连接 LLM Wiki 后端：${err instanceof Error ? err.message : String(err)}`)
  }
  if (res.status === 401) throw new UnauthorizedError()
  const json = (await res.json().catch(() => ({}))) as Record<string, unknown>
  if (!res.ok || json.ok === false) {
    const message = typeof json.error === "string" ? json.error : `HTTP ${res.status}`
    throw new Error(message)
  }
  return json as T
}

export interface ApiProject {
  id: string
  name: string
  path: string
  current: boolean
}

export interface ApiFileNode {
  name: string
  path: string
  isDir: boolean
  children?: ApiFileNode[]
}

export const api = {
  projects: () =>
    request<{ projects: ApiProject[]; currentProject: ApiProject | null }>("/api/v1/projects"),

  files: (projectId: string) =>
    request<{ files: ApiFileNode[] }>(
      `/api/v1/projects/${encodeURIComponent(projectId)}/files?root=wiki&recursive=true`,
    ),

  fileContent: (projectId: string, path: string) =>
    request<{ path: string; content: string }>(
      `/api/v1/projects/${encodeURIComponent(projectId)}/files/content?path=${encodeURIComponent(path)}`,
    ),

  chatSessions: (projectId: string) =>
    request<{
      sessions: { sessionId: string; updatedAt: number; messages: { role: string; content: string }[] }[];
    }>(`/api/v1/projects/${encodeURIComponent(projectId)}/chat/sessions`),
}

/** The files API returns Windows verbatim absolute paths (`//?/D:/...`);
 * normalize to project-relative forward-slash paths. */
export function toRelativePath(absPath: string, projectPath: string): string {
  let p = absPath.replace(/^\/\/\?\//, "").replace(/\\/g, "/")
  const base = projectPath.replace(/\/$/, "").toLowerCase()
  if (p.toLowerCase().startsWith(base + "/")) p = p.slice(base.length + 1)
  return p
}
