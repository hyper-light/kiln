# UI mockups

Interactive mockups of the designs in `docs/ui/*.md`, for the person to see and press before anything is
built. Open any of them in a browser; each links to the others.

- `runners.html`: docs/ui/runners-engine.md (P2.3, P3.2).
- `agents.html`: docs/ui/agents.md (P5.1, P5.2, P5.4).
- `agentfile.html`: docs/ui/agentfile-structured.md (P4.5, P4.6, P5.3, P5.4).
- `experiments.html`: docs/ui/experiments.md (P7.1, P7.2).

`assets/kiln.css` is kiln's own stylesheet as `npx vite build` compiled it from `ui/src` at b2fc6cebb (its main
sheet, then the Agentfiles view's). `assets/fonts.css` and the four `.woff2` files are the Geist and Geist
Mono latin faces of that build. `assets/mockup.css` writes kiln's React-only components as plain classes
(Button's variants, Segmented, the agent page's tab rail), and adds the classes each new surface would add.
`assets/mockup.js` fakes the server's answers. None of it is production code; the data is sample data.
