/** A plan's steps as the server finds them (crates/kiln/src/plans/mod.rs `steps`): the items of its top-level lists. */
export function planSteps(body: string): string[] {
  const out: string[] = [];
  let fence = false;
  for (const line of body.split(/\r?\n/)) {
    const trimmed = line.trimStart();
    if (trimmed.startsWith("```") || trimmed.startsWith("~~~")) {
      fence = !fence;
      continue;
    }
    if (fence) continue;
    const top = line.length === trimmed.length;
    const item = top ? listItem(trimmed) : null;
    if (item !== null) {
      if (item) out.push(item);
    } else if (!top && trimmed && out.length > 0) {
      out[out.length - 1] += ` ${trimmed}`;
    }
  }
  return out;
}

function listItem(line: string): string | null {
  let rest: string | null = null;
  for (const m of ["- ", "* ", "+ "]) {
    if (line.startsWith(m)) {
      rest = line.slice(2);
      break;
    }
  }
  if (rest === null) {
    const m = /^(\d{1,3})[.)] (.*)$/.exec(line);
    if (!m) return null;
    rest = m[2] ?? "";
  }
  return rest.replace(/^\[[ xX]\] /, "").trim();
}

/** A plan's text without the heading it opens with when that heading is its title (shown apart). */
export function withoutTitle(body: string, title: string): string {
  const lines = body.trim().split(/\r?\n/);
  const m = /^#{1,6}\s+(.*?)\s*#*\s*$/.exec(lines[0]?.trim() ?? "");
  if (!m || m[1] !== title.trim()) return body.trim();
  return lines.slice(1).join("\n").trim();
}

/** A plan's items as markdown, each under its own heading (crates/kiln/src/plans `markdown`). */
export function itemsMarkdown(items: readonly { title: string; description: string; examples: string; acceptance: string; testing: string }[]): string {
  return items
    .map((it, i) => {
      const parts = [`## ${i + 1}. ${it.title.trim()}`];
      if (it.description.trim()) parts.push(it.description.trim());
      for (const [name, v] of [
        ["Examples", it.examples],
        ["Acceptance criteria", it.acceptance],
        ["Testing", it.testing],
      ] as const) {
        if (v.trim()) parts.push(`**${name}**\n\n${v.trim()}`);
      }
      return parts.join("\n\n");
    })
    .join("\n\n");
}

/** A plan as one markdown file: its title as the heading, its text, then its items. */
export function planMarkdown(title: string, body: string, items: Parameters<typeof itemsMarkdown>[0] = []): string {
  const text = withoutTitle(body, title);
  const rest = itemsMarkdown(items);
  return `# ${title}\n\n${[text, rest].filter(Boolean).join("\n\n")}\n`;
}
