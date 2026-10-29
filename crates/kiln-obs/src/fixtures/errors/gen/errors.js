// Stack traces as Node's tracers report them (dd-trace-js's `error.stack`, OpenTelemetry's
// `exception.stacktrace`: the error's `stack`), for kiln's error grouping tests.
class Cart {
  constructor(items) { this.items = items; }
  total() {
    for (const i of this.items) if (i < 0) throw new Error(`bad item ${i} in cart "c-${i * 7}"`);
    return this.items.reduce((a, b) => a + b, 0);
  }
}
function checkout(order) { return new Cart([1, 2, -order]).total(); }
async function fetchOrder(id) { await new Promise((r) => setTimeout(r, 1)); throw new TypeError(`order ${id} not found`); }
async function handler(id) { return await fetchOrder(id); }
function parse(body) { return JSON.parse(body); }
function cases() {
  const out = [];
  const run = async (name, f) => { try { await f(); } catch (e) { out.push(`---- ${name}\n${e.stack}\n`); } };
  return (async () => {
    await run("app", () => checkout(42));
    await run("app-again", () => checkout(7));
    await run("async", () => handler(5));
    await run("builtin", () => parse("{not json"));
    await run("anonymous", () => [1, 0].map(function (v) { if (v === 0) throw new RangeError("zero (at index 1)"); return v; }));
    process.stdout.write(out.join(""));
  })();
}
cases();
