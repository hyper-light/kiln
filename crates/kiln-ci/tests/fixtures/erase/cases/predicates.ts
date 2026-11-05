function a(x: unknown): asserts x is string {}
function p(x: unknown): x is string { return true }
const q = (x: unknown): x is number => typeof x === "number";
