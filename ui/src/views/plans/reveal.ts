/** The plan review at the top of an agent's Plan tab (review.tsx), brought into view once that tab shows. */
export const REVIEW_ATTR = "data-plan-review";

export function revealReview() {
  requestAnimationFrame(() => document.querySelector<HTMLElement>(`[${REVIEW_ATTR}]`)?.scrollIntoView({ block: "start", behavior: "smooth" }));
}
