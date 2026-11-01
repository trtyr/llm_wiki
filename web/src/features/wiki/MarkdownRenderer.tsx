import { useEffect, useMemo, useRef } from "react"
import ReactMarkdown from "react-markdown"
import remarkGfm from "remark-gfm"
import remarkMath from "remark-math"
import rehypeKatex from "rehype-katex"
import mermaid from "mermaid"
import { useNavigate } from "@tanstack/react-router"
import { rewriteWikilinks, type WikiLookup } from "./wikilinks"

let mermaidReady = false
function ensureMermaid(): void {
  if (mermaidReady) return
  mermaid.initialize({ startOnLoad: false, securityLevel: "loose", theme: "neutral" })
  mermaidReady = true
}

function MermaidBlock({ code }: { code: string }) {
  const ref = useRef<HTMLDivElement>(null)
  useEffect(() => {
    let cancelled = false
    ensureMermaid()
    const id = "mermaid-" + Math.random().toString(36).slice(2)
    mermaid
      .render(id, code)
      .then(({ svg }) => {
        if (!cancelled && ref.current) ref.current.innerHTML = svg
      })
      .catch((err) => {
        if (!cancelled && ref.current)
          ref.current.textContent = "Mermaid 语法错误：" + (err instanceof Error ? err.message : String(err))
      })
    return () => {
      cancelled = true
    }
  }, [code])
  return <div ref={ref} className="mermaid-box" />
}

function InternalLink({ href, children }: { href: string; children: React.ReactNode }) {
  const navigate = useNavigate()
  if (href.startsWith("/w/")) {
    return (
      <a
        href={href}
        className="wiki-link"
        onClick={(event) => {
          event.preventDefault()
          void navigate({ to: href })
        }}
      >
        {children}
      </a>
    )
  }
  const external = href.startsWith("http")
  return (
    <a href={href} target={external ? "_blank" : undefined} rel={external ? "noreferrer" : undefined}>
      {children}
    </a>
  )
}

export function MarkdownRenderer({
  content,
  projectId,
  lookup,
}: {
  content: string
  projectId: string
  lookup: WikiLookup
}) {
  const prepared = useMemo(() => rewriteWikilinks(content, projectId, lookup), [content, projectId, lookup])
  return (
    <div className="wiki-content">
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkMath]}
        rehypePlugins={[rehypeKatex]}
        components={{
          a: ({ href, children }) => (
            <InternalLink href={href ?? "#"}>{children}</InternalLink>
          ),
          pre: ({ children }) => {
            const child = Array.isArray(children) ? children[0] : children
            const className =
              (child as { props?: { className?: string } } | undefined)?.props?.className ?? ""
            if (className.includes("language-mermaid")) {
              const code = String((child as { props?: { children?: unknown } })?.props?.children ?? "").replace(
                /\n$/,
                "",
              )
              return <MermaidBlock code={code} />
            }
            return <pre>{children}</pre>
          },
        }}
      >
        {prepared}
      </ReactMarkdown>
    </div>
  )
}

/** Extract (h2|h3) outline entries for the right panel. */
export function extractOutline(content: string): { level: number; text: string }[] {
  const out: { level: number; text: string }[] = []
  let inCode = false
  for (const line of content.split(/\r?\n/)) {
    if (line.trimStart().startsWith("```")) inCode = !inCode
    if (inCode) continue
    const m = /^(#{2,3})\s+(.+)$/.exec(line)
    if (m) out.push({ level: m[1].length, text: m[2].trim() })
  }
  return out
}
