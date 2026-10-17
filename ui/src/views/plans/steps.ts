import type { Criterion, PlanItem, PlanTest } from "@/api/plans";

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

/** A number and its unit, as a sentence has them: "200 ms", "99.9%" (crates/kiln/src/plans/criteria.rs). */
function amount(value: string, unit = ""): string {
  const v = value.trim();
  const u = unit.trim();
  if (!u) return v;
  return /^[%°x×]/.test(u) ? `${v}${u}` : `${v} ${u}`;
}

const words = (s: string) => s.split(/\s+/).filter(Boolean).join(" ");

/** A criterion as the sentence an agent reads (mirrors `Criterion::sentence`). */
export function criterionSentence(c: Criterion): string {
  if (c.kind === "qualitative") return words(c.text);
  const t = c.target.trim();
  const a = amount(c.value, c.unit);
  const base = c.baseline?.trim() || "the baseline";
  const pct = `${c.value.trim()}%`;
  const s =
    c.op === "lt"
      ? `${t} is less than ${a}`
      : c.op === "le"
        ? `${t} is at most ${a}`
        : c.op === "eq"
          ? `${t} is exactly ${a}`
          : c.op === "ne"
            ? `${t} is not ${a}`
            : c.op === "ge"
              ? `${t} is at least ${a}`
              : c.op === "gt"
                ? `${t} is more than ${a}`
                : c.op === "between"
                  ? `${t} is between ${a} and ${amount(c.value2 ?? "", c.unit)}`
                  : c.op === "within"
                    ? `${t} is within ${amount(c.value2 ?? "", c.unit)} of ${a}`
                    : c.op === "rises_at_least"
                      ? `${t} rises by at least ${pct} compared with ${base}`
                      : c.op === "falls_at_least"
                        ? `${t} falls by at least ${pct} compared with ${base}`
                        : c.op === "rises_at_most"
                          ? `${t} rises by no more than ${pct} compared with ${base}`
                          : `${t} falls by no more than ${pct} compared with ${base}`;
  return c.measured?.trim() ? `${s}, measured by ${words(c.measured)}` : s;
}

/** A test as one sentence: its scenario, then what it passes by (mirrors `Test::sentence`). */
export function testSentence(t: PlanTest): string {
  const scenario = words(t.scenario);
  const pass = t.criteria.map(criterionSentence).join("; ");
  return scenario && pass ? `${scenario} — passes when ${pass}` : scenario || pass;
}

/** A plan's steps as markdown, each under its own heading (crates/kiln/src/plans `markdown`). */
export function itemsMarkdown(items: readonly PlanItem[]): string {
  return items
    .map((it, i) => {
      const parts = [`## ${i + 1}. ${it.title.trim()}`];
      if (it.description.trim()) parts.push(it.description.trim());
      if (it.examples.trim()) parts.push(`**Examples**\n\n${it.examples.trim()}`);
      if (it.acceptance.length) parts.push(`**Acceptance criteria**\n\n${it.acceptance.map((c) => `- ${criterionSentence(c)}`).join("\n")}`);
      if (it.testing.length) parts.push(`**Tests**\n\n${it.testing.map((t) => `- ${testSentence(t)}`).join("\n")}`);
      return parts.join("\n\n");
    })
    .join("\n\n");
}

/** A plan as one markdown file: its title as the heading, its text, then its items. */
export function planMarkdown(title: string, body: string, items: readonly PlanItem[] = []): string {
  const text = withoutTitle(body, title);
  const rest = itemsMarkdown(items);
  return `# ${title}\n\n${[text, rest].filter(Boolean).join("\n\n")}\n`;
}
