import { add } from "./fixture.ts";

Deno.test("adds", () => {
  if (add(1, 2) !== 3) {
    throw new Error("1 + 2 is 3");
  }
});
