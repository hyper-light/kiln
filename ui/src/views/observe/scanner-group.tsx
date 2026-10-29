/**
 * A scanning group of the Sensitive Data Scanner (docs/observe.md §8.9): its filter, its rules
 * (each what it finds, where it looks, what replaces a match, the keywords that must be near one,
 * the tags a match puts on its record), and a record tried on it before it is kept.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Pencil, Plus, Trash2 } from "lucide-react";
import { Link, useNavigate, useParams } from "@/lib/router";
import { observeApi, type PipelineResult, type ScanReplacement, type ScanningGroupIn, type ScanningRule, type StandardPattern } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Panel, SelectField, TextField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { swapped } from "@/lib/utils";
import { scanningGroupRes, standardPatternsRes } from "./parts";

const BLANK: ScanningGroupIn = { name: "", filter: { query: "" }, is_enabled: true, rules: [] };
const REPLACEMENTS: { value: ScanReplacement; label: string }[] = [
  { value: "replacement_string", label: "A string in its place" },
  { value: "partial_replacement_from_beginning", label: "Its first characters hidden" },
  { value: "partial_replacement_from_end", label: "Its last characters hidden" },
  { value: "hash", label: "A hash of it" },
  { value: "none", label: "Nothing: the record tagged" },
];
const PRIORITIES = ["critical", "high", "medium", "low", "info"];
const CATEGORY: Record<string, string> = { credit_card: "Credit card", bank_account: "Bank account", pii: "Personal", network: "Network", credentials: "Credentials" };

const list = (s: string) =>
  s
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);

export default function ScanningGroupPage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page max-w-[1100px]">
      <Loadable key={id} label="the scanning group" fallback={<Hold height={480} label="Loading the scanning group" />}>
        {id === "new" ? (
          <>
            <title>New scanning group · Observe · kiln</title>
            <Header crumb="New" />
            <Editor initial={BLANK} />
          </>
        ) : (
          <Existing id={id} />
        )}
      </Loadable>
    </div>
  );
}

function Header({ crumb, title }: { crumb: string; title?: string }) {
  return (
    <header className="page-intro">
      <div className="min-w-0">
        <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Sensitive data", to: "/observe/scanner" }, { label: crumb }]} />
        <h1>{title ?? "New scanning group"}</h1>
      </div>
    </header>
  );
}

function Existing({ id }: { id: string }) {
  const g = useRes(scanningGroupRes(id)).data;
  return (
    <>
      <title>{`${g.name} · Sensitive data · kiln`}</title>
      <Header crumb={g.name} title={g.name} />
      <Editor initial={g} id={g.id} />
    </>
  );
}

function Editor({ initial, id }: { initial: ScanningGroupIn; id?: string }) {
  const patterns = useRes(standardPatternsRes).data;
  const [g, setG] = React.useState<ScanningGroupIn>(() => ({ name: initial.name, description: initial.description, filter: initial.filter, is_enabled: initial.is_enabled, rules: initial.rules }));
  const [busy, setBusy] = React.useState(false);
  const navigate = useNavigate();
  const save = async () => {
    setBusy(true);
    try {
      const saved = id ? await observeApi.updateScanningGroup(id, g) : await observeApi.createScanningGroup(g);
      invalidate("observe/scanner/groups");
      invalidate(`observe/scanner/group/${saved.id}`);
      toast({ title: id ? "Saved" : "Made", description: `${saved.name}: records taken from now are scanned by it.`, tone: "succeeded" });
      if (!id) navigate(`/observe/scanner/${encodeURIComponent(saved.id)}`);
    } catch (e) {
      toast({ title: "Couldn't save it", description: errorMessage(e), tone: "failed" });
    } finally {
      setBusy(false);
    }
  };
  return (
    <form
      className="space-y-8"
      onSubmit={(e) => {
        e.preventDefault();
        if (g.name.trim()) void save();
      }}
    >
      <div className="grid max-w-[860px] grid-cols-1 gap-3 sm:grid-cols-2">
        <TextField label="Name" value={g.name} onChange={(e) => setG({ ...g, name: e.target.value })} placeholder="payments" required />
        <TextField
          label="Filter"
          value={g.filter.query}
          onChange={(e) => setG({ ...g, filter: { query: e.target.value } })}
          placeholder="service:checkout"
          hint="A log search: the records it scans. Empty, every record."
          className="font-mono"
          spellCheck={false}
        />
      </div>
      <Rules rules={g.rules} patterns={patterns} onChange={(rules) => setG({ ...g, rules })} />
      <TryIt group={g} />
      <div className="flex justify-end gap-2">
        <Button variant="ghost" asChild>
          <Link to="/observe/scanner">Cancel</Link>
        </Button>
        <Button type="submit" disabled={busy || !g.name.trim()}>
          {id ? "Save" : "Make the group"}
        </Button>
      </div>
    </form>
  );
}

/** What a rule does to what it finds, in a few words. */
function action(r: ScanningRule): string {
  const t = r.text_replacement;
  switch (t.type) {
    case "replacement_string":
      return `replaced with ${t.replacement_string || "[sensitive_data]"}`;
    case "partial_replacement_from_beginning":
      return `its first ${t.number_of_chars ?? 0} hidden`;
    case "partial_replacement_from_end":
      return `its last ${t.number_of_chars ?? 0} hidden`;
    case "hash":
      return "hashed";
    default:
      return "tagged";
  }
}

function Rules({ rules, patterns, onChange }: { rules: ScanningRule[]; patterns: StandardPattern[]; onChange: (r: ScanningRule[]) => void }) {
  const [editing, setEditing] = React.useState<{ at: number | null; r: ScanningRule } | null>(null);
  const named = (r: ScanningRule) => (r.standard_pattern_id ? (patterns.find((p) => p.id === r.standard_pattern_id)?.name ?? r.standard_pattern_id) : "A pattern of its own");
  const fresh = (): ScanningRule => {
    const p = patterns[0];
    return {
      name: p?.name ?? "",
      standard_pattern_id: p?.id,
      namespaces: [],
      excluded_namespaces: [],
      tags: [],
      is_enabled: true,
      text_replacement: { type: "replacement_string" },
      priority: p?.priority ?? 3,
    };
  };
  return (
    <Panel
      eyebrow="Rules"
      title="What it finds"
      description="Every rule looks through each record the filter picks; of two matches that overlap, the first (then the rule of higher priority) is replaced."
      actions={
        <Button variant="ghost" onClick={() => setEditing({ at: null, r: fresh() })}>
          <Plus /> Add a rule
        </Button>
      }
    >
      {rules.length === 0 ? (
        <p className="empty-line">None yet: add a rule for what this group should find.</p>
      ) : (
        <ol className="divide-y divide-white/[0.06]">
          {rules.map((r, i) => (
            <li key={r.id ?? i} className="flex min-w-0 items-center gap-3 py-2">
              <span className="cell-mono w-5 shrink-0 text-[#8c8f98]">{i + 1}</span>
              <span className="min-w-0 flex-1">
                <span className="block truncate text-[13px] text-[#e4e5e7]">
                  {r.name}
                  {!r.is_enabled && <span className="ml-2 text-[11px] text-[#8c8f98]">off</span>}
                </span>
                <span className="block truncate text-[12px] text-[#8c8f98]">
                  {named(r)}, {action(r)}
                  {r.namespaces.length > 0 ? ` · in ${r.namespaces.join(", ")}` : ""}
                </span>
              </span>
              <span className="shrink-0 whitespace-nowrap">
                <Button variant="ghost" size="xs" aria-label={`Move ${r.name} up`} disabled={i === 0} onClick={() => onChange(swapped(rules, i, i - 1))}>
                  <ArrowUp />
                </Button>
                <Button variant="ghost" size="xs" aria-label={`Move ${r.name} down`} disabled={i === rules.length - 1} onClick={() => onChange(swapped(rules, i, i + 1))}>
                  <ArrowDown />
                </Button>
                <Button variant="ghost" size="xs" aria-label={`Edit ${r.name}`} onClick={() => setEditing({ at: i, r })}>
                  <Pencil />
                </Button>
                <Button variant="ghost-danger" size="xs" aria-label={`Remove ${r.name}`} onClick={() => onChange(rules.filter((_, j) => j !== i))}>
                  <Trash2 />
                </Button>
              </span>
            </li>
          ))}
        </ol>
      )}
      {editing && (
        <RuleDialog
          rule={editing.r}
          patterns={patterns}
          adding={editing.at === null}
          onClose={() => setEditing(null)}
          onSave={(r) => {
            onChange(editing.at === null ? [...rules, r] : rules.map((x, j) => (j === editing.at ? r : x)));
            setEditing(null);
          }}
        />
      )}
    </Panel>
  );
}

interface RuleForm {
  name: string;
  standard: string;
  pattern: string;
  replacement: ScanReplacement;
  text: string;
  chars: string;
  namespaces: string;
  excluded: string;
  keywords: string;
  window: string;
  recommended: boolean;
  tags: string;
  priority: string;
  on: boolean;
}

const formOf = (r: ScanningRule): RuleForm => ({
  name: r.name,
  standard: r.standard_pattern_id ?? "",
  pattern: r.pattern ?? "",
  replacement: r.text_replacement.type,
  text: r.text_replacement.replacement_string ?? "",
  chars: String(r.text_replacement.number_of_chars ?? 4),
  namespaces: r.namespaces.join(", "),
  excluded: r.excluded_namespaces.join(", "),
  keywords: (r.included_keyword_configuration?.keywords ?? []).join(", "),
  window: String(r.included_keyword_configuration?.character_count ?? 30),
  recommended: r.included_keyword_configuration?.use_recommended_keywords ?? false,
  tags: r.tags.join(", "),
  priority: String(r.priority),
  on: r.is_enabled,
});

function RuleDialog({ rule, patterns, adding, onClose, onSave }: { rule: ScanningRule; patterns: StandardPattern[]; adding: boolean; onClose: () => void; onSave: (r: ScanningRule) => void }) {
  const [f, setF] = React.useState<RuleForm>(() => formOf(rule));
  const set = (patch: Partial<RuleForm>) => setF((old) => ({ ...old, ...patch }));
  const std = patterns.find((p) => p.id === f.standard);
  const partial = f.replacement === "partial_replacement_from_beginning" || f.replacement === "partial_replacement_from_end";
  const chars = Number(f.chars);
  const window = Number(f.window);
  const keywords = list(f.keywords);
  const ok = f.name.trim() !== "" && (std !== undefined || f.pattern.trim() !== "") && (!partial || (Number.isInteger(chars) && chars > 0)) && Number.isInteger(window) && window >= 1 && window <= 50;
  const save = () =>
    onSave({
      ...(rule.id ? { id: rule.id } : {}),
      name: f.name.trim(),
      ...(std ? { standard_pattern_id: std.id } : { pattern: f.pattern.trim() }),
      namespaces: list(f.namespaces),
      excluded_namespaces: list(f.excluded),
      tags: list(f.tags),
      is_enabled: f.on,
      text_replacement: { type: f.replacement, ...(f.replacement === "replacement_string" && f.text.trim() ? { replacement_string: f.text } : {}), ...(partial ? { number_of_chars: chars } : {}) },
      ...(keywords.length > 0 || f.recommended ? { included_keyword_configuration: { keywords, character_count: window, use_recommended_keywords: f.recommended } } : {}),
      priority: Number(f.priority),
    });
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={adding ? "Add a rule" : `Edit ${rule.name}`} description="What it finds, where, and what becomes of each match.">
        <div className="space-y-4">
          <TextField label="Name" value={f.name} onChange={(e) => set({ name: e.target.value })} placeholder="card numbers" />
          <SelectField
            label="Finds"
            value={f.standard}
            onChange={(v) => {
              const p = patterns.find((x) => x.id === v);
              // A new rule takes its pattern's name and priority until they are changed.
              const named = patterns.some((x) => x.name === f.name) || f.name.trim() === "";
              set({ standard: v, ...(p && named ? { name: p.name } : {}), ...(p ? { priority: String(p.priority) } : {}) });
            }}
            options={[...patterns.map((p) => ({ value: p.id, label: `${CATEGORY[p.category] ?? p.category} · ${p.name}` })), { value: "", label: "A pattern of my own" }]}
          />
          {std ? (
            <p className="field-hint -mt-2">
              {std.description}
              {std.keywords_required && ` Only with one of its keywords near: ${std.keywords.join(", ")}.`}
            </p>
          ) : (
            <TextField
              label="Pattern"
              value={f.pattern}
              onChange={(e) => set({ pattern: e.target.value })}
              placeholder="order-[0-9]{8}"
              className="font-mono"
              spellCheck={false}
              hint="A regular expression (no look-around); a group named v is what is replaced: user=(?P<v>\w+)"
            />
          )}
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <SelectField label="Each match" value={f.replacement} onChange={(v) => set({ replacement: REPLACEMENTS.find((r) => r.value === v)?.value ?? "none" })} options={REPLACEMENTS} />
            {f.replacement === "replacement_string" && (
              <TextField label="Replaced with" value={f.text} onChange={(e) => set({ text: e.target.value })} placeholder="[sensitive_data]" spellCheck={false} />
            )}
            {partial && <TextField label="Characters hidden" type="number" min={1} value={f.chars} onChange={(e) => set({ chars: e.target.value })} />}
          </div>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField
              label="Looks in"
              value={f.namespaces}
              onChange={(e) => set({ namespaces: e.target.value })}
              placeholder="everywhere"
              spellCheck={false}
              hint="The message, or attributes by path (http.url), beneath them too"
            />
            <TextField label="Not in" value={f.excluded} onChange={(e) => set({ excluded: e.target.value })} placeholder="nowhere" spellCheck={false} />
          </div>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-[1fr_9rem]">
            <TextField
              label="Keywords"
              value={f.keywords}
              onChange={(e) => set({ keywords: e.target.value })}
              placeholder="none"
              spellCheck={false}
              hint="A match counts only with one of them just before it."
            />
            <TextField label="Within (characters)" type="number" min={1} max={50} value={f.window} onChange={(e) => set({ window: e.target.value })} />
          </div>
          {std && std.keywords.length > 0 && !std.keywords_required && (
            <label className="flex items-start gap-2 text-[13px] text-[#c8cad0]">
              <input type="checkbox" className="check mt-0.5" checked={f.recommended} onChange={(e) => set({ recommended: e.target.checked })} /> Use its keywords too: {std.keywords.join(", ")}
            </label>
          )}
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <TextField
              label="Tags"
              value={f.tags}
              onChange={(e) => set({ tags: e.target.value })}
              placeholder={std ? std.tags.join(", ") : "sensitive_data:<its name>"}
              spellCheck={false}
              hint="key:value, put on each record it matches"
            />
            <SelectField label="Priority" value={f.priority} onChange={(v) => set({ priority: v })} options={PRIORITIES.map((p, i) => ({ value: String(i + 1), label: `${i + 1} · ${p}` }))} />
          </div>
          <label className="flex items-center gap-2 text-[13px] text-[#c8cad0]">
            <input type="checkbox" className="check" checked={f.on} onChange={(e) => set({ on: e.target.checked })} /> On
          </label>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button disabled={!ok} onClick={save}>
              {adding ? "Add it" : "Keep it"}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function TryIt({ group }: { group: ScanningGroupIn }) {
  const [message, setMessage] = React.useState("");
  const [out, setOut] = React.useState<{ r: PipelineResult } | { error: string } | null>(null);
  const run = async () => {
    try {
      // Its rules on the message: which records its filter picks is another question.
      const [r] = await observeApi.tryScanner({ ...group, name: group.name.trim() || "tried", is_enabled: true, filter: { query: "" } }, [{ message }]);
      setOut(r ? { r } : { error: "no record came back" });
    } catch (e) {
      setOut({ error: errorMessage(e) });
    }
  };
  const tags = out && "r" in out ? Object.entries(out.r.attributes) : [];
  return (
    <Panel eyebrow="Try it" title="A message through its rules" description="As they are now, saved or not, whatever the filter picks: nothing is kept, and a hash is the one this kiln makes.">
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-[1fr_auto] sm:items-end">
        <TextField
          label="Message"
          value={message}
          onChange={(e) => setMessage(e.target.value)}
          placeholder="paid with 4111 1111 1111 1111, mail jane@example.com"
          className="font-mono"
          spellCheck={false}
        />
        <Button variant="secondary" disabled={!message.trim()} onClick={() => void run()}>
          Try it
        </Button>
      </div>
      {out && (
        <div className="mt-4" role="status">
          {"error" in out ? (
            <p className="text-[13px] text-[#e2c4c6]">{out.error}</p>
          ) : out.r.message === message && tags.length === 0 ? (
            <p className="empty-line">Nothing found: no rule that is on matches it.</p>
          ) : (
            <dl className="grid grid-cols-1 gap-x-4 font-mono text-[12px] sm:grid-cols-[minmax(0,auto)_minmax(0,1fr)] sm:gap-y-1">
              {([["message", out.r.message], ...tags] as [string, string][]).map(([k, v]) => (
                <React.Fragment key={k}>
                  <dt className="truncate text-[#8c8f98]">{k}</dt>
                  <dd className="mb-1.5 text-[#e4e5e7] [overflow-wrap:anywhere] sm:mb-0">{v}</dd>
                </React.Fragment>
              ))}
            </dl>
          )}
        </div>
      )}
    </Panel>
  );
}
