import { clsx, type ClassValue } from "clsx";

/**
 * Class names, joined (falsy ones left out). Not merged: two classes setting the same thing would
 * both apply, and which wins would be up to the stylesheet's order, so a component's classes and
 * its callers' never overlap (a variant or size says it instead). Merging would cost every page
 * the size of tailwind-merge.
 */
/** `a` with its items at `i` and `j` swapped (as it was where either is not there). */
export function swapped<T>(a: readonly T[], i: number, j: number): T[] {
  const out = a.slice();
  const x = out[i];
  const y = out[j];
  if (x === undefined || y === undefined) return out;
  out[i] = y;
  out[j] = x;
  return out;
}

export function cn(...inputs: ClassValue[]) {
  return clsx(inputs);
}
