/**
 * Runners → Offers' data (GET /api/reach/offers, docs/reach.md §2.1): read again only when the
 * server says its version moved past the one shown (`offers` events carry it), and then with the
 * ETag of what's shown, so an unchanged page costs a 304 and nothing more. No timer reads it.
 */
import { ApiError, setAuthed } from "@/api/client";
import type { OffersView, ServerEvent } from "@/api/wire";
import type { Res } from "@/lib/data";

/** What each page was last read as, with its ETag. */
const held = new Map<string, { etag: string; view: OffersView }>();

/** The path of the page after `after` (the first page without). */
export function offersPath(after: string | null): string {
  return after ? `/api/reach/offers?after=${encodeURIComponent(after)}` : "/api/reach/offers";
}

async function load(path: string, signal: AbortSignal): Promise<OffersView> {
  const had = held.get(path);
  const res = await fetch(path, { credentials: "same-origin", signal, headers: had ? { "if-none-match": had.etag } : undefined });
  if (res.status === 304 && had) return had.view;
  if (res.status === 401) {
    setAuthed(false);
    throw new ApiError(401, "Not signed in");
  }
  if (!res.ok) throw new ApiError(res.status, (await res.text()).slice(0, 300) || res.statusText);
  const view = (await res.json()) as OffersView;
  const etag = res.headers.get("etag");
  if (etag) held.set(path, { etag, view });
  return view;
}

/** Whether event `ev` leaves `data` as it is: an `offers` event of a version it has already. */
export function current(data: OffersView, ev: ServerEvent): OffersView | undefined {
  return ev.type === "offers" && ev.version <= data.version ? data : undefined;
}

/** The view's page after `after`. */
export function offersRes(after: string | null): Res<OffersView> {
  const path = offersPath(after);
  return { key: `reach/offers${after ? `?after=${after}` : ""}`, fetch: (s) => load(path, s), opts: { on: ["offers"], apply: current } };
}
