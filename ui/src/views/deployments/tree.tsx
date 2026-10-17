/**
 * An app's resource tree, two ways. On a wide screen, a graph read left to right: the app, its
 * objects, what they own, joined by quiet lines, sized to the width it has so it never scrolls
 * sideways. Anywhere narrower, the same tree as an indented list with guide lines, each owner
 * folding its children. Large sets of alike siblings show as one node ("12 pods") that opens in
 * place. Choosing a node is the caller's: the graph's details go under it, the list's under the
 * row chosen.
 */
import * as React from "react";
import { ChevronRight } from "lucide-react";

import type { CdNode } from "@/api/types";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { age, flatten, healthCounts, troubled, type GroupItem, type Item } from "./model";
import { HEALTH_TONE, KindBadge, NodeMarks, nodeLine } from "./parts";

const NODE_H = 54;
const ROW_GAP = 10;
const COL_GAP = 52;
const NODE_MAX = 300;
/** Narrower than this, the graph gives way to the list. */
export const NODE_MIN = 156;

export interface AppMark {
  name: string;
  health: string;
  sync: string;
}

interface Placed {
  key: string;
  item: Item | null;
  depth: number;
  y: number;
  parent: string | null;
  /** The node's other owners in the graph (dashed lines). */
  also: string[];
}

/** Rows top to bottom, each owner centred on what it owns; the app at the far left. */
function place(roots: Item[]) {
  const out: Placed[] = [];
  let row = 0;
  const at = (it: Item | null, key: string, depth: number, parent: string | null, kids: Item[]): number => {
    const own: Placed = { key, item: it, depth, y: 0, parent, also: [] };
    out.push(own);
    // An opened group is a header followed by its members, in its own column.
    const list: { it: Item; key: string }[] = [];
    for (const k of kids) {
      list.push({ it: k, key: k.id });
      if (k.type === "group" && k.open) for (const m of k.children) list.push({ it: m, key: m.id });
    }
    if (list.length === 0) {
      own.y = row++ * (NODE_H + ROW_GAP);
      return own.y;
    }
    const ys = list.map((l) => at(l.it, l.key, depth + 1, key, l.it.type === "node" ? l.it.children : []));
    own.y = ((ys[0] ?? 0) + (ys[ys.length - 1] ?? 0)) / 2;
    return own.y;
  };
  at(null, "app", 0, null, roots);
  // The app heads its tree: at the top, where reading starts, not halfway down a long column.
  const first = out.find((p) => p.parent === "app");
  const app = out[0];
  if (app && first) app.y = first.y;
  const ids = new Set(out.map((p) => p.key));
  for (const p of out) {
    if (p.item?.type === "node") p.also = p.item.node.parents.filter((u) => u !== p.parent && ids.has(u));
  }
  const depth = Math.max(0, ...out.map((p) => p.depth));
  return { placed: out, rows: row, depth };
}

/** Whether a graph of these roots fits `width` with nodes no narrower than NODE_MIN. */
export function graphFits(roots: Item[], width: number) {
  const { depth } = place(roots);
  const cols = depth + 1;
  return width > 0 && (width - COL_GAP * (cols - 1)) / cols >= NODE_MIN;
}

export function TreeGraph({
  roots,
  app,
  width,
  selected,
  onSelect,
  onGroup,
}: {
  roots: Item[];
  app: AppMark;
  width: number;
  selected: string | null;
  onSelect: (n: CdNode) => void;
  onGroup: (g: GroupItem) => void;
}) {
  const now = useNow();
  const { placed, rows, depth } = React.useMemo(() => place(roots), [roots]);
  const cols = depth + 1;
  const nodeW = Math.min(NODE_MAX, Math.floor((width - COL_GAP * (cols - 1)) / cols));
  const height = Math.max(NODE_H, rows * (NODE_H + ROW_GAP) - ROW_GAP);
  const pos = new Map(placed.map((p) => [p.key, p]));
  const x = (d: number) => d * (nodeW + COL_GAP);
  const edge = (from: Placed, to: Placed) => {
    const x1 = x(from.depth) + nodeW;
    const y1 = from.y + NODE_H / 2;
    const x2 = x(to.depth);
    const y2 = to.y + NODE_H / 2;
    const mx = (x1 + x2) / 2;
    return `M${x1},${y1} C${mx},${y1} ${mx},${y2} ${x2},${y2}`;
  };
  const lines: { d: string; key: string; also: boolean }[] = [];
  for (const p of placed) {
    const from = p.parent ? pos.get(p.parent) : undefined;
    if (from) lines.push({ d: edge(from, p), key: `${p.parent}>${p.key}`, also: false });
    for (const a of p.also) {
      const other = pos.get(a);
      if (other) lines.push({ d: edge(other, p), key: `${a}>${p.key}~`, also: true });
    }
  }
  return (
    <div className="cd-graph" style={{ height }} role="group" aria-label="Resource tree">
      <svg className="cd-graph-lines" width={width} height={height} aria-hidden="true">
        {lines.map((l) => (
          <path key={l.key} d={l.d} className="dag-edge" data-also={l.also || undefined} />
        ))}
      </svg>
      {placed.map((p) => {
        const style = { left: x(p.depth), top: p.y, width: nodeW, height: NODE_H };
        if (!p.item) {
          return (
            <div key="app" className="cd-gnode cd-gapp" style={style}>
              <span className="cd-gnode-top">
                <span className="cd-kind">app</span>
                <span className="cd-gnode-name">{app.name}</span>
              </span>
              <span className="cd-gnode-line">
                <NodeMarks n={{ health: app.health as CdNode["health"], sync: app.sync as CdNode["sync"], managed: true, hook: false }} />
              </span>
            </div>
          );
        }
        const it = p.item;
        if (it.type === "group") {
          return (
            <button
              key={p.key}
              type="button"
              className="cd-gnode cd-ggroup"
              data-open={it.open || undefined}
              style={style}
              onClick={() => onGroup(it)}
              aria-expanded={it.open}
              title={it.open ? `Fold the ${it.members.length} ${it.noun}` : `Show the ${it.members.length} ${it.noun}`}
            >
              <span className="cd-gnode-top">
                <KindBadge kind={it.kind} />
                <span className="cd-gnode-name">
                  {it.members.length} {it.noun}
                </span>
                <span className="cd-ggroup-act">{it.open ? "Fold" : "Show"}</span>
              </span>
              <span className="cd-gnode-line">{healthCounts(it.members)}</span>
            </button>
          );
        }
        const n = it.node;
        return (
          <button
            key={p.key}
            type="button"
            className="cd-gnode"
            style={style}
            data-selected={selected === n.id || undefined}
            data-context={it.context || undefined}
            data-missing={n.missing || undefined}
            data-tone={troubled(n) ? (n.health === "Degraded" || n.health === "Missing" ? "fail" : "warn") : undefined}
            data-health={n.health ? HEALTH_TONE[n.health] : undefined}
            aria-pressed={selected === n.id}
            onClick={() => onSelect(n)}
            title={`${n.kind} ${n.namespace ? `${n.namespace}/` : ""}${n.name}${n.message ? `\n${n.message}` : ""}`}
          >
            <span className="cd-gnode-top">
              <KindBadge kind={n.kind} group={n.group} />
              <span className="cd-gnode-name">{n.name}</span>
              <NodeMarks n={n} words={false} />
            </span>
            <span className="cd-gnode-line">{nodeLine(n, age(n.created_at, now)) || (n.managed ? n.sync : "")}</span>
          </button>
        );
      })}
    </div>
  );
}

/**
 * The tree as an indented list: guide lines for depth, a fold for each owner, and the details
 * of the row chosen right under it (`details`).
 */
export function TreeList({
  roots,
  namespace,
  selected,
  onSelect,
  onGroup,
  details,
}: {
  roots: Item[];
  /** The app's namespace: said only of objects elsewhere. */
  namespace: string;
  selected: string | null;
  onSelect: (n: CdNode) => void;
  onGroup: (g: GroupItem) => void;
  details: React.ReactNode;
}) {
  const now = useNow();
  const [closed, setClosed] = React.useState<ReadonlySet<string>>(() => new Set());
  const rows = React.useMemo(() => flatten(roots, closed), [roots, closed]);
  const toggle = (id: string) =>
    setClosed((c) => {
      const next = new Set(c);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  // The object chosen isn't a row here (its owner is folded): its details open on top.
  const inView = rows.some((r) => r.item.type === "node" && r.item.node.id === selected);
  return (
    <ul className="cd-tlist" aria-label="Resource tree">
      {!inView && details ? <li className="cd-trow-details">{details}</li> : null}
      {rows.map(({ item, depth }) => {
        const guides = <span className="cd-guides" style={{ "--depth": depth } as React.CSSProperties} aria-hidden="true" />;
        if (item.type === "group") {
          return (
            <li key={`g:${item.id}`} className="cd-trow cd-trow-group">
              {guides}
              <span className="cd-fold-space" aria-hidden="true" />
              <button type="button" className="cd-trow-main" onClick={() => onGroup(item)} aria-expanded={item.open}>
                <span className="cd-trow-top">
                  <KindBadge kind={item.kind} />
                  <span className="cd-trow-name">
                    {item.members.length} {item.noun}
                  </span>
                  <span className="cd-ggroup-act">{item.open ? "Fold" : "Show"}</span>
                </span>
                <span className="cd-trow-line">{healthCounts(item.members)}</span>
              </button>
            </li>
          );
        }
        const n = item.node;
        const has = item.children.length > 0;
        const isOpen = !closed.has(item.id);
        const chosen = selected === n.id;
        return (
          <React.Fragment key={item.id}>
            <li
              className="cd-trow"
              data-selected={chosen || undefined}
              data-context={item.context || undefined}
              data-tone={troubled(n) ? (n.health === "Degraded" || n.health === "Missing" ? "fail" : "warn") : undefined}
            >
              {guides}
              {has ? (
                <button
                  type="button"
                  className="cd-fold"
                  onClick={() => toggle(item.id)}
                  aria-expanded={isOpen}
                  aria-label={`${isOpen ? "Fold" : "Unfold"} what ${n.name} owns`}
                >
                  <ChevronRight size={13} className={cn("cd-chev", isOpen && "cd-chev-open")} aria-hidden="true" />
                </button>
              ) : (
                <span className="cd-fold-space" aria-hidden="true" />
              )}
              <button type="button" className="cd-trow-main" onClick={() => onSelect(n)} aria-expanded={chosen}>
                <span className="cd-trow-top">
                  <KindBadge kind={n.kind} group={n.group} />
                  <span className="cd-trow-name">{n.name}</span>
                  <NodeMarks n={n} />
                </span>
                <span className="cd-trow-line">
                  {[n.namespace && n.namespace !== namespace ? n.namespace : "", nodeLine(n, age(n.created_at, now))].filter(Boolean).join(" · ") || n.kind}
                </span>
              </button>
            </li>
            {chosen && details ? <li className="cd-trow-details">{details}</li> : null}
          </React.Fragment>
        );
      })}
    </ul>
  );
}
