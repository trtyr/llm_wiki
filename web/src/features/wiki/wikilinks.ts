// Wikilink resolution for the M0 reading experience.
// A [[target]] / [[target|label]] / [[target#section|label]] reference is
// resolved against the wiki file index (built from the /files tree) and
// rewritten into a plain markdown link the renderer can handle.

export type WikiLookup = (target: string) => string | null

/** Build a lookup from relative wiki paths (e.g. `wiki/concepts/x.md`). */
export function buildWikiLookup(relatives: string[]): WikiLookup {
  const byRelNoExt = new Map<string, string>()
  const byBase = new Map<string, string>()
  for (const rel of relatives) {
    if (!rel.toLowerCase().endsWith(".md")) continue
    const noExt = rel.slice(0, -3)
    // Full relative id ("concepts/x" and "wiki/concepts/x" both usable).
    byRelNoExt.set(noExt.toLowerCase(), rel)
    byRelNoExt.set(rel.toLowerCase(), rel)
    const base = noExt.split("/").pop() ?? ""
    if (base && !byBase.has(base.toLowerCase())) byBase.set(base.toLowerCase(), rel)
  }
  return (target: string): string | null => {
    const cleaned = target.trim().replace(/\.md$/i, "")
    if (!cleaned) return null
    const lower = cleaned.toLowerCase()
    return (
      byRelNoExt.get(lower) ??
      byRelNoExt.get("wiki/" + lower) ??
      byBase.get(lower) ??
      null
    )
  }
}

export interface WikilinkParts {
  matched: boolean
  md: string
}

/** Rewrite every wikilink in `body` into a markdown link whose href points
 * at the SPA route of the resolved page. Unresolvable links stay visible
 * as inert text (rendered with a dashed style via .wiki-link-unresolved). */
export function rewriteWikilinks(
  body: string,
  projectId: string,
  lookup: WikiLookup,
): string {
  return body.replace(
    /\[\[([^\]|]+?)(?:#([^\]|]+))?(?:\|([^\]]+))?\]\]/g,
    (_whole, rawTarget: string, section: string | undefined, label: string | undefined) => {
      const target = String(rawTarget).trim()
      const text = (label ?? target).trim()
      const rel = lookup(target)
      if (!rel) return `**${text}**`
      let href = `/w/${encodeURIComponent(projectId)}/${rel
        .split("/")
        .map(encodeURIComponent)
        .join("/")}`
      if (section) href += "#" + encodeURIComponent(section.trim())
      return `[${text}](${href})`
    },
  )
}

/** Split YAML frontmatter from the body; returns flat key/values we can
 * show in the meta bar (tags, type, sources count). */
export function splitFrontmatter(
  raw: string,
): { meta: Record<string, string>; body: string } {
  const match = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?/.exec(raw)
  if (!match) return { meta: {}, body: raw }
  const meta: Record<string, string> = {}
  for (const line of match[1].split(/\r?\n/)) {
    const kv = /^([A-Za-z_-]+)\s*:\s*(.*)$/.exec(line.trim())
    if (!kv) continue
    meta[kv[1].toLowerCase()] = kv[2].trim().replace(/^\[|\]$/g, "").trim()
  }
  return { meta, body: raw.slice(match[0].length) }
}
