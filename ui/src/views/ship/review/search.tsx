/**
 * The Files section's search (docs/ci.md W26-R07, step 5): a vorpal-backed box whose hits jump to
 * their lines — a changed line's in the diff, another file's in the whole-file view — while the
 * query's own letters mark the lines shown and dim the rest.
 */
import * as React from "react";
import { Search } from "lucide-react";
import type { SearchHit, SearchResult } from "@/api/review";
import type { Res } from "@/lib/data";
import { useRes } from "@/lib/data";
import "./review.css";

export interface Jump {
  file: string;
  line: number;
  /** The whole-file view, for a hit outside the changed files. */
  whole?: boolean;
}

export function SearchBox({ q, onQ, hits, onJump }: { q: string; onQ: (q: string) => void; hits: Res<SearchResult> | null; onJump: (j: Jump) => void }) {
  return (
    <div className="rv-search">
      <label className="rv-q">
        <Search size={13} aria-hidden="true" />
        <input value={q} onChange={(e) => onQ(e.target.value)} placeholder="Search · vorpal" aria-label="Search the change" autoComplete="off" />
      </label>
      {q.trim() && hits && (
        <React.Suspense fallback={<span className="rv-hits-w">…</span>}>
          <Hits res={hits} onJump={onJump} />
        </React.Suspense>
      )}
    </div>
  );
}

function Hits({ res, onJump }: { res: Res<SearchResult>; onJump: (j: Jump) => void }) {
  const r = useRes(res).data;
  return (
    <div className="rv-hits" role="list">
      <span className="rv-hits-w">
        {r.total} match{r.total === 1 ? "" : "es"}
        {r.truncated ? "+" : ""}
      </span>
      {r.hits.slice(0, 12).map((h, i) => (
        <button key={i} type="button" className="rv-hit" data-change={h.changed || undefined} onClick={() => onJump(jumpOf(h))}>
          <span className="rv-hit-at">
            {h.path}
            {h.symbol ? ` · ${h.symbol}` : ""}:{h.line}
          </span>
          <code>{h.text.trim()}</code>
        </button>
      ))}
    </div>
  );
}

/** Where a hit jumps: into the diff when it's a file the change touches, else the whole file. */
function jumpOf(h: SearchHit): Jump {
  return { file: h.path, line: h.line, whole: !h.in_change && !h.changed };
}
