/**
 * What each capture name of a highlights query looks like: one of a few classes (syntax.css, in
 * hyperlight-site's code colours). Queries name captures as tree-sitter, Neovim and Helix do
 * (`function.method.call`, `markup.heading`, `text.title`); a name takes the class of the longest
 * part of it that has one (`function.method.call` → `function.method` → `function`).
 *
 * `""` is plain: the code's own colour, over whatever is under it (an identifier in a template
 * string's `${}` isn't the string's colour). `null` isn't a highlight at all and leaves what's under
 * it as it is: Neovim's spell-checking marks, captures a query keeps for its predicates (`@_name`),
 * and names no theme knows.
 */

const CLASSES: Record<string, string | null> = {
  comment: "hl-com",

  keyword: "hl-kw",
  "keyword.directive": "hl-meta",
  conditional: "hl-kw",
  repeat: "hl-kw",
  include: "hl-kw",
  exception: "hl-kw",
  storageclass: "hl-kw",
  "type.qualifier": "hl-kw",
  "variable.builtin": "hl-kw",
  "parameter.builtin": "hl-kw",
  preproc: "hl-meta",
  define: "hl-meta",

  string: "hl-str",
  character: "hl-str",
  "string.escape": "hl-esc",
  "string.regex": "hl-esc",
  "string.regexp": "hl-esc",
  "string.special.regex": "hl-esc",
  "string.special.symbol": "hl-num",
  "string.special.key": "hl-prop",
  "string.special.url": "hl-link",
  "string.special.uri": "hl-link",
  "string.special.path": "hl-link",
  "character.special": "hl-esc",
  escape: "hl-esc",

  number: "hl-num",
  float: "hl-num",
  boolean: "hl-num",
  constant: "hl-num",
  "constant.macro": "hl-meta",
  "constant.character.escape": "hl-esc",
  symbol: "hl-num",

  function: "hl-fn",
  method: "hl-fn",
  "function.macro": "hl-meta",
  "function.special": "hl-meta",
  constructor: "hl-type",

  type: "hl-type",
  class: "hl-type",
  interface: "hl-type",
  struct: "hl-type",
  enum: "hl-type",
  namespace: "hl-type",
  module: "hl-type",

  property: "hl-prop",
  field: "hl-prop",
  "variable.member": "hl-prop",
  "variable.other.member": "hl-prop",
  "property.value": "hl-str",

  attribute: "hl-meta",
  annotation: "hl-meta",
  decorator: "hl-meta",
  label: "hl-meta",
  tag: "hl-tag",
  "tag.attribute": "hl-prop",
  "tag.delimiter": "hl-punct",

  operator: "hl-punct",
  punctuation: "hl-punct",
  delimiter: "hl-punct",
  "punctuation.special": "hl-esc",

  variable: "",
  parameter: "",
  embedded: "",
  none: "",
  text: "",
  markup: "",

  "markup.heading": "hl-strong",
  "text.title": "hl-strong",
  "markup.strong": "hl-strong",
  "text.strong": "hl-strong",
  "markup.italic": "hl-em",
  "text.emphasis": "hl-em",
  "markup.strikethrough": "hl-strike",
  "text.strike": "hl-strike",
  "markup.link": "hl-link",
  "text.uri": "hl-link",
  "text.reference": "hl-link",
  "markup.raw": "hl-str",
  "text.literal": "hl-str",
  "markup.list": "hl-punct",
  "markup.quote": "hl-com",
};

/** Names that are never highlights: Neovim's marks for spell-checking and concealing. */
const IGNORED = new Set(["spell", "nospell", "conceal", "error"]);

const memo = new Map<string, string | null>();

/** A capture name's class: `""` plain, `null` none (see above). */
export function classOf(capture: string): string | null {
  const hit = memo.get(capture);
  if (hit !== undefined) return hit;
  let out: string | null = null;
  if (!capture.startsWith("_") && !capture.startsWith("injection.") && !capture.startsWith("local.") && !IGNORED.has(capture)) {
    for (let name = capture; name; name = name.slice(0, Math.max(0, name.lastIndexOf(".")))) {
      const c = CLASSES[name];
      if (c !== undefined) {
        out = c;
        break;
      }
    }
  }
  memo.set(capture, out);
  return out;
}

/** The classes a highlighted line can carry, in the order their indices run (0 is plain). */
export const CLASS_LIST: readonly string[] = ["", ...new Set(Object.values(CLASSES).filter((c): c is string => !!c))];
export const CLASS_INDEX = new Map(CLASS_LIST.map((c, i) => [c, i]));
