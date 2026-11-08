/**
 * Ship's first read, apart from the rest of api/ship.ts: the document's early script
 * (lib/early-script.ts) is in every page's first chunk, so it takes only this, nothing else of Ship's.
 */

/** How many rows Ship's list asks for: one a repository, as many as a page shows. */
export const SHIP_ROWS = 100;

/** Ship's first read as the page opens, no facet chosen: `shipSearch({ by: "repo", limit: SHIP_ROWS })` (tests/early-reads.test.ts holds them alike). */
export const SHIP_FIRST_READ = `/api/ship/commits?by=repo&limit=${SHIP_ROWS}`;
