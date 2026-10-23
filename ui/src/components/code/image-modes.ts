/**
 * The code editor's image modes (docs/ci.md W21-R01, W13-R11): a Dockerfile, as CodeMirror's own
 * mode reads one, and an Agentfile, shards' (a Dockerfile's vocabulary and its own directives:
 * `AGENT`, `HARNESS`, `SKILL`, `MCP`, `NETWORK`, `CONNECT`, `ATTACH`), read the same way.
 */
import { dockerFile } from "@codemirror/legacy-modes/mode/dockerfile";
import { simpleMode } from "@codemirror/legacy-modes/mode/simple-mode";

export { dockerFile };

const SHELLS = ["run", "cmd", "entrypoint", "shell"];
const OTHERS = ["arg", "maintainer", "label", "env", "add", "copy", "volume", "user", "workdir", "onbuild", "stopsignal", "healthcheck"];
/** shards' own. */
const AGENTFILE = ["agent", "harness", "skill", "mcp", "network", "connect", "attach"];

/** A token per group, as the mode's own JavaScript writes them: `null` for no style (its types have
 *  no room for that in a list). */
const tok = (...t: (string | null)[]) => t as string[];

const all = ["from", "expose", ...SHELLS, ...OTHERS, ...AGENTFILE].join("|");
const onlyLine = new RegExp(`^(\\s*)(${all})(\\s*)(#.*)?$`, "i");
const withArgs = new RegExp(`^(\\s*)(${all})(\\s+)`, "i");

export const agentFile = simpleMode({
  start: [
    { regex: /^\s*#.*$/, sol: true, token: "comment" },
    { regex: /^(\s*)\b(from)\b/i, token: tok(null, "keyword"), sol: true, next: "from" },
    { regex: onlyLine, token: tok(null, "keyword", null, "error"), sol: true },
    { regex: new RegExp(`^(\\s*)(${SHELLS.join("|")})(\\s+\\[)`, "i"), token: tok(null, "keyword", null), sol: true, next: "array" },
    { regex: /^(\s*)(expose)(\s+)/i, token: tok(null, "keyword", null), sol: true, next: "expose" },
    { regex: withArgs, token: tok(null, "keyword", null), sol: true, next: "arguments" },
    { regex: /./, token: null },
  ],
  from: [
    { regex: /\s*$/, token: null, next: "start" },
    { regex: /(\s*)(#.*)$/, token: tok(null, "error"), next: "start" },
    { regex: /(\s*\S+\s+)(as)/i, token: tok(null, "keyword"), next: "start" },
    { token: null, next: "start" },
  ],
  single: [
    { regex: /(?:[^\\']|\\.)/, token: "string" },
    { regex: /'/, token: "string", pop: true },
  ],
  double: [
    { regex: /(?:[^\\"]|\\.)/, token: "string" },
    { regex: /"/, token: "string", pop: true },
  ],
  array: [
    { regex: /\]/, token: null, next: "start" },
    { regex: /"(?:[^\\"]|\\.)*"?/, token: "string" },
  ],
  expose: [
    { regex: /\d+$/, token: "number", next: "start" },
    { regex: /[^\d]+$/, token: null, next: "start" },
    { regex: /\d+/, token: "number" },
    { regex: /[^\d]+/, token: null },
    { token: null, next: "start" },
  ],
  arguments: [
    { regex: /^\s*#.*$/, sol: true, token: "comment" },
    { regex: /"(?:[^\\"]|\\.)*"?$/, token: "string", next: "start" },
    { regex: /"/, token: "string", push: "double" },
    { regex: /'(?:[^\\']|\\.)*'?$/, token: "string", next: "start" },
    { regex: /'/, token: "string", push: "single" },
    { regex: /[^#"']+[\\`]$/, token: null },
    { regex: /[^#"']+$/, token: null, next: "start" },
    { regex: /[^#"']+/, token: null },
    { token: null, next: "start" },
  ],
  languageData: { commentTokens: { line: "#" } },
});
