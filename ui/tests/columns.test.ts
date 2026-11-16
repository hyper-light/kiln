/**
 * W21-R02: a list written as columns reads back as the list. The fixture is what the server's writer writes of
 * these rows (`crates/kiln/src/wire/columns.rs` holds it to that), each of its forms among its columns.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fromColumns, isColumns, type Columns } from "../src/api/columns.ts";

const fixture = JSON.parse(readFileSync(new URL("./fixtures/columns.json", import.meta.url), "utf8")) as { rows: Record<string, unknown>[]; columns: Columns };

test("W21-R02 a list read back from its columns is the list, each row its keys with their values and no other", () => {
  assert.ok(isColumns(fixture.columns));
  const forms = fixture.columns.cols.map(([key, column]) => `${key}:${Object.keys(column).sort().join("+")}`);
  // Each form is among the fixture's columns: plain, said once, mostly one value, begun alike, objects as columns, only some rows.
  for (const form of ["created_at:v", "kind:of+v", "status:d+v+x", "cwd:pre+v", "admission:at+d+v+x", "last_turn:at+cols", "turn_started:at+v"])
    assert.ok(forms.includes(form), `${form} among ${forms.join(" ")}`);
  const rows = fromColumns<Record<string, unknown>>(fixture.columns);
  assert.deepStrictEqual(rows, fixture.rows);
  // A key a row never said isn't given it, and a null it said is.
  assert.equal("last_turn" in rows[3]!, false);
  assert.equal("note" in rows[5]! && rows[5]!.note === null, true);
});

test("W21-R02 a value said once for several rows is each row's own", () => {
  const rows = fromColumns<{ admission?: { epoch: number }; approvals: unknown[] }>(fixture.columns);
  assert.deepStrictEqual(rows[0]!.admission, rows[1]!.admission);
  assert.notStrictEqual(rows[0]!.admission, rows[1]!.admission);
  rows[0]!.admission!.epoch = 9;
  assert.equal(rows[1]!.admission!.epoch, 0);
});

test("W21-R02 a list that isn't columns is told apart: a server from before them answers the list itself", () => {
  for (const read of [fixture.rows, [], null, "x", 3, { rows: 2 }, { cols: [] }, { rows: "2", cols: [] }]) assert.equal(isColumns(read), false, JSON.stringify(read));
  assert.deepStrictEqual(fromColumns({ rows: 0, cols: [] }), []);
  assert.deepStrictEqual(fromColumns({ rows: 2, cols: [] }), [{}, {}]);
});
