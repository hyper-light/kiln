import { test } from "node:test";
import assert from "node:assert/strict";
import { fileStamp, toCsv } from "../src/lib/download.ts";

test("CSV quotes only the fields that need it, and keeps their quotes and line breaks", () => {
  const csv = toCsv(
    ["time", "who", "detail"],
    [
      ["2026-10-21T04:00:00.000Z", "admin", "plain"],
      ["2026-10-21T04:00:01.000Z", "agent:a, for admin", 'said "no"\nthen left'],
      ["2026-10-21T04:00:02.000Z", null, undefined],
    ],
  );
  assert.equal(csv, 'time,who,detail\r\n2026-10-21T04:00:00.000Z,admin,plain\r\n2026-10-21T04:00:01.000Z,"agent:a, for admin","said ""no""\nthen left"\r\n2026-10-21T04:00:02.000Z,,\r\n');
});

test("a file's stamp is its local minute", () => {
  assert.equal(fileStamp(new Date(2026, 9, 20, 21, 53, 12)), "20261020-2153");
  assert.equal(fileStamp(new Date(2026, 0, 2, 3, 4)), "20260102-0304");
});
