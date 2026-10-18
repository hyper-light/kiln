import { clsx, type ClassValue } from "clsx";

/**
 * Class names, joined (falsy ones left out). Not merged: two classes setting the same thing would
 * both apply, and which wins would be up to the stylesheet's order, so a component's classes and
 * its callers' never overlap (a variant or size says it instead). Merging would cost every page
 * the size of tailwind-merge.
 */
export function cn(...inputs: ClassValue[]) {
  return clsx(inputs);
}
