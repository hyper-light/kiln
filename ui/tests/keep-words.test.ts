import assert from "node:assert/strict";
import { test } from "node:test";
import { draftOf, hoursSaid, merge, rulesOf, rulesSaid, scheduleSaid } from "../src/views/keep/words.ts";

test("a scope's rules fill in from the scope above, field by field, 0 and '' saying none", () => {
  const every = { newest: 5, hours: 168, schedule: "0 3 * * *", free_percent: 10 };
  assert.deepEqual(merge({ newest: 0, schedule: "" }, every), { newest: 0, hours: 168, schedule: "", free_percent: 10 });
  assert.deepEqual(merge({}, every), every);
});

test("rules read as a sentence, and say when nothing goes on its own", () => {
  assert.equal(
    rulesSaid({ newest: 5, hours: 168, schedule: "0 3 * * *", free_percent: 10, beyond: true }),
    "The newest 5 stay, and any used in the last 7 days; the rest go every day at 03:00 (UTC), and whenever free space falls under 10%. With too little room still, the least recently used of those kept go too, until there's room. One in use, or used in the last ten minutes, always stays.",
  );
  assert.equal(rulesSaid({ newest: 3 }), "The newest 3 stay. Nothing goes on its own: no schedule or room limit is set.");
  assert.match(rulesSaid({ newest: 2, hours: 36, both: true, free_gib: 50 }), /^The newest 2 stay while each was used in the last 36 hours; the rest go whenever free space falls under 50 GiB\./);
  assert.match(rulesSaid({ schedule: "15 * * * *", timezone: "Europe/Berlin" }), /^No rule keeps a copy; every copy goes at `15 \* \* \* \*` \(Europe\/Berlin\)\./);
  assert.equal(scheduleSaid("0 * * * *"), "every hour (UTC)");
  assert.equal(hoursSaid(24), "a day");
  assert.equal(hoursSaid(48), "2 days");
  assert.equal(hoursSaid(5), "5 hours");
});

test("a draft of rules comes back as the same rules, and a bad one says what's wrong", () => {
  const rules = { newest: 4, hours: 72, both: true, schedule: "0 3 * * 0", timezone: "UTC", free_percent: 15, beyond: false };
  const d = draftOf(rules, false);
  assert.equal(d.hours.unit, "days");
  assert.equal(d.hours.value, "3");
  assert.deepEqual(rulesOf(d), { rules });
  // Unset stays unset (the scope above's); at the top, none is what unset is, so nothing is set.
  assert.deepEqual(rulesOf(draftOf({}, false)), { rules: {} });
  assert.deepEqual(rulesOf(draftOf({}, true), true), { rules: {} });
  assert.deepEqual(rulesOf(draftOf({ newest: 0, schedule: "" }, false)), { rules: { newest: 0, schedule: "" } });
  assert.deepEqual(rulesOf(draftOf({ newest: 3, both: true }, true), true), { rules: { newest: 3, both: true } });
  const bad = { ...draftOf({}, false), newest: { mode: "set" as const, value: "two" } };
  assert.ok("error" in rulesOf(bad));
  const cron = { ...draftOf({}, false), schedule: { mode: "set" as const, value: "daily" } };
  assert.ok("error" in rulesOf(cron));
});
