# W29-R27: kiln's type erasure beside Node's

Plan 01a1abb6's step 1. Each case under `cases/` is a TypeScript file. `node.json` is what Node's own
type stripping made of each one: `module.stripTypeScriptTypes(code, { mode: "strip" })`, the
JavaScript (`js`), or the error it refused with (`refused`, `message`). kiln's tests
(`crates/kiln-ci/src/wf/code/tests.rs`) erase every case with `kiln_ci::wf::code::erase` and hold it
to Node's, byte for byte. Where Node refuses with `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`, kiln refuses
with `KCI1033` at each place the case's first line names (`// refused at <line>:<column>, …`), since
Node's errors don't say where.

The cases cover what R27 erases and what it refuses:

- annotations, interfaces, type aliases, `declare`, namespaces of types only;
- `import type`, `export type` and type-only specifiers;
- type parameters and arguments, `as`, `satisfies`, the non-null and definite-assignment `!`;
- classes' modifiers, `abstract`, overloads and index signatures;
- `this` parameters, arrow functions and type predicates.

They also hold Node's two repairs:

- a `;` where an erased statement would let the statements around it join (`asi-*`);
- an arrow function's `)` moved to the line of its `=>` when its return type spans lines.

Node replaces each character with whitespace of its own UTF-8 length, keeping line ends and tabs
(`widths`, `crlf`). kiln differs from Node in one place, which its own test holds: it puts a
`;` before a template holding `${` after an erased statement, where Node's output tags the template
with the statement before. The refusals are `enum`, namespaces holding code, parameter properties,
`import x =`, `export =` and `<T>x`.

## Recording

`record.mjs` rewrites `node.json` from `cases/`: `node record.mjs`, with Node 24. Recorded with
Node v24.21.0 on 2026-11-05 on adacachy, exit 0.
