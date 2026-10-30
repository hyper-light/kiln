// W21-AC01's two page-wide asserts (docs/ci.md), for every check to make on each page it visits:
// no element scrolls sideways (the page, or anything in it that would scroll its content left and
// right) but a code block, which keeps each line on one line (`[data-code-scroll]`: a comment's
// code in Ship's Files and Conversation), and nothing is `position: fixed` but the shell's own (its
// skip link, its toasts and what waits on you above them, a tooltip, the overlay, dialog and drawer
// it opens). Each is said by what the element is, at most eight a kind.

/** The shell's own fixed layers. */
const SHELL = ".skip-link, .toast-viewport, .rq-viewport, [role=tooltip], .overlay, .dialog, .drawer";

export async function layoutProblems(page) {
  return page.evaluate((shell) => {
    const name = (el) => {
      const cls = typeof el.className === "string" ? el.className.trim().split(/\s+/).filter(Boolean).slice(0, 2) : [];
      const label = el.getAttribute("aria-label");
      return `${el.tagName.toLowerCase()}${el.id ? `#${el.id}` : ""}${cls.length ? `.${cls.join(".")}` : ""}${label ? `[${label}]` : ""}`;
    };
    const sideways = [];
    const fixed = [];
    const doc = document.scrollingElement;
    if (doc && doc.scrollWidth > window.innerWidth + 1) sideways.push(`the page (${doc.scrollWidth - window.innerWidth}px)`);
    for (const el of document.querySelectorAll("body *")) {
      const cs = getComputedStyle(el);
      if ((cs.overflowX === "auto" || cs.overflowX === "scroll") && el.clientWidth > 0 && el.scrollWidth > el.clientWidth + 1 && !el.closest("[data-code-scroll]"))
        sideways.push(`${name(el)} (${el.scrollWidth - el.clientWidth}px)`);
      // Only what can be seen: a screen reader's live region kept off screen isn't a layer.
      const r = el.getBoundingClientRect();
      const seen = r.width > 0 && r.height > 0 && r.right > 0 && r.bottom > 0 && r.left < window.innerWidth && r.top < window.innerHeight;
      if (cs.position === "fixed" && seen && !el.closest(shell)) fixed.push(name(el));
    }
    return { sideways: sideways.slice(0, 8), fixed: fixed.slice(0, 8) };
  }, SHELL);
}

/** Both asserts as `check` takes them: `(what, ok, detail)` each. */
export async function checkLayout(check, who, page) {
  const p = await layoutProblems(page);
  check(who, "nothing scrolls sideways", p.sideways.length === 0, p.sideways.join(", "));
  check(who, "nothing fixed but the shell's own", p.fixed.length === 0, p.fixed.join(", "));
}

/**
 * A page's errors as the checks count them, kept as they come (`base`: the server the check
 * drives). A fetch the page was still making when it went to another page, WebKit reports as "Fetch
 * API cannot load <url> due to access control checks", and Playwright takes that for a page error,
 * though the page caught it (Chromium only says the request was cut off). Nothing else makes a
 * request to the page's own server fail those checks, so that report isn't counted; one for any
 * other server is.
 */
export function pageErrors(page, base) {
  const errors = [];
  const own = new URL(base).host;
  page.on("pageerror", (e) => {
    const said = String(e);
    const cut = /^Fetch API cannot load https?:\s?\/{1,2}([^/\s]+)\S* due to access control checks\.$/.exec(said);
    if (cut?.[1] !== own) errors.push(said);
  });
  return errors;
}
