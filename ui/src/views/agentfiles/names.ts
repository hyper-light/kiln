/**
 * An Agentfile's name, as the pages take one (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 31): an
 * image's, as the server holds it (`agentfiles::name_problem`), and a copy's, free among those taken.
 */

/** An image's name: lowercase letters and digits joined by `.`, `_`, `__` or runs of `-`, 255 at the most. */
export const named = (n: string) => /^[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*$/.test(n) && n.length <= 255;

/** A name for a copy of `name` among `taken`: its own, else the first `-2`, `-3` … free. */
export function copyName(name: string, taken: readonly string[]): string {
  const have = new Set(taken);
  if (!have.has(name)) return name;
  for (let n = 2; ; n++) if (!have.has(`${name}-${n}`)) return `${name}-${n}`;
}
