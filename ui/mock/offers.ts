/**
 * Runners → Offers (GET /api/reach/offers, docs/reach.md §2.2): four runners (one taking work, one
 * strained, one before offers, one offline) and six groups of waiting work, a page as long as the
 * fleet. Its ETag is its version, answered 304 when the page has it. For the browser tests:
 * `POST /api/mock/offers/change` changes what waits and says so (an `offers` event of the next version);
 * `POST /api/mock/offers/same` says the version the page has already; `GET /api/mock/offers/reads` counts
 * the page's reads and its 304s.
 */
import type http from "node:http";
import type { OfferAmounts, OfferGroup, OfferRunner, OffersView, ServerEvent } from "../src/api/wire/index.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  emit: (ev: ServerEvent) => void;
}

const GiB = 1024 ** 3;
const boot = Date.now();
let version = 1;
let extra = 0;
const reads = { full: 0, unchanged: 0 };

const amt = (cpus: number, gib: number, gpu = 0, diskGib = 0): OfferAmounts => ({ cpu_milli: cpus * 1000, mem: gib * GiB, disk: diskGib * GiB, gpu });
const SMALL = amt(4, 8);
const BIG = amt(16, 32);
const GPU = amt(8, 32, 1);

function runners(heard: number): OfferRunner[] {
  return [
    {
      name: "adacachy",
      platform: "linux/x86_64",
      connected: true,
      said: true,
      heard,
      budget: amt(28, 96, 0, 900),
      room: amt(10, 40, 0, 600),
      queued: [
        { key: "interactive", count: 1 },
        { key: "batch", count: 2 },
      ],
      health: { taking: true },
      answers: [
        { ask: SMALL, answer: "now", derived: false },
        { ask: BIG, answer: "waits", reason: "line", soonest: { unit: "min", count: 3, from: heard }, latest: { unit: "min", count: 41, from: heard }, derived: false },
        { ask: GPU, answer: "never", reason: "past its budget of GPUs", derived: true },
      ],
    },
    {
      name: "a1b2c3d4e5f60718293a4b5c6d7e8f90",
      label: "adas-macbook-pro-with-a-very-long-name-that-a-runner-may-well-have.local",
      platform: "macos/aarch64",
      connected: true,
      said: true,
      heard,
      budget: amt(18, 48),
      room: amt(2, 6),
      queued: [],
      health: { taking: false, why: "memory pressure 41% (some avg10), past the gate's guard" },
      answers: [
        { ask: SMALL, answer: "waits", reason: "strained", derived: false },
        { ask: BIG, answer: "waits", reason: "strained", derived: true },
        { ask: GPU, answer: "never", reason: "past its budget of GPUs", derived: true },
      ],
    },
    { name: "metal-pi-04", platform: "linux/aarch64", connected: true, said: false, queued: [], health: { taking: true, why: "says nothing of its admission (a runner before offers)" }, answers: [] },
    { name: "fedora.attlocal.net", platform: "linux/x86_64", connected: false, said: false, queued: [], health: { taking: false, why: "offline" }, answers: [] },
  ];
}

const group = (need: string, ask: OfferAmounts, runs: number, reasons: [string, number][], by: [string, number][], minsAgo: number): OfferGroup => ({
  need,
  ask,
  runs,
  by_class: by.map(([key, count]) => ({ key, count })),
  reasons: reasons.map(([key, count]) => ({ key, count })),
  since: boot - minsAgo * 60_000,
  cursor: `${ask.cpu_milli}.${ask.mem}.${ask.disk}.${ask.gpu}.${need}`,
});

function groups(): OfferGroup[] {
  const all = [
    group(
      "linux/amd64",
      BIG,
      3 + extra,
      [
        ["capacity", 2 + extra],
        ["line", 1],
      ],
      [["batch", 3 + extra]],
      12,
    ),
    group("linux/amd64", SMALL, 1, [["line", 1]], [["interactive", 1]], 2),
    group(
      "linux/amd64 +gpu",
      GPU,
      2,
      [["capability", 2]],
      [
        ["land", 1],
        ["batch", 1],
      ],
      47,
    ),
    group(
      "macos",
      SMALL,
      4,
      [["strained", 4]],
      [
        ["interactive", 2],
        ["batch", 2],
      ],
      9,
    ),
    group("", amt(1, 1), 1, [["retry", 1]], [["background", 1]], 1),
    group("windows", SMALL, 1, [["no-runner", 1]], [["batch", 1]], 130),
  ];
  return all.sort((a, b) => (a.need === b.need ? a.ask.cpu_milli - b.ask.cpu_milli : a.need < b.need ? -1 : 1));
}

function view(after: string | null): OffersView {
  const rs = runners(boot);
  const all = groups();
  const limit = rs.length;
  const start = after ? all.findIndex((g) => g.cursor === after) + 1 : 0;
  const page = all.slice(start, start + limit);
  const more = start + limit < all.length;
  return {
    version,
    now: Date.now(),
    runners: rs,
    sizes: [SMALL, BIG, GPU],
    waiting: { runs: all.reduce((a, g) => a + g.runs, 0), groups_total: all.length, groups: page, next: more ? page[page.length - 1]?.cursor : undefined, limit },
  };
}

export function handleOffers(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): boolean {
  const m = req.method ?? "GET";
  if (url.pathname === "/api/reach/offers" && m === "GET") {
    const after = url.searchParams.get("after");
    const etag = `W/"offers-${boot}-${version}-${after ?? ""}"`;
    if (req.headers["if-none-match"] === etag) {
      reads.unchanged++;
      res.writeHead(304, { etag, "cache-control": "private, no-cache" });
      res.end();
      return true;
    }
    reads.full++;
    res.setHeader("etag", etag);
    res.setHeader("cache-control", "private, no-cache");
    ctx.json(res, 200, view(after));
    return true;
  }
  if (url.pathname === "/api/mock/offers/change" && m === "POST") {
    extra++;
    version++;
    ctx.emit({ type: "offers", version });
    ctx.json(res, 200, { version });
    return true;
  }
  if (url.pathname === "/api/mock/offers/same" && m === "POST") {
    ctx.emit({ type: "offers", version });
    ctx.json(res, 200, { version });
    return true;
  }
  if (url.pathname === "/api/mock/offers/reads" && m === "GET") {
    ctx.json(res, 200, reads);
    return true;
  }
  return false;
}
