# Research

What an agent finds out, kept in kiln with where it found it, and graded by kiln: so whatever an
agent presents carries its sources and a grade a person can weigh. A plan made from a goal is
researched first (its planning phase: [plans.md](plans.md), "Planning a goal"); research stands
alone too, for any question a person asks an agent to look into.

## What an agent records

- **Sources**, each with what kind of thing it is (below) and where it is: a file and its lines
  (`crates/kiln/src/convo/store.rs:396-410`), a URL (with its `#section`), a DOI (`doi:10.1145/…`), a
  record of kiln's (`plan:<id>`, `agent:<id>`), or for a measurement the command it ran. Optionally
  its author, when it was published, the machine a file is on, its origin (who stands behind it,
  when several sources share one: a vendor's docs and its blog), and whether it's out of date for
  this question.
- **Findings**, each a claim with its citations (the source, the exact words quoted from it, where
  they are) and any counterclaims the agent found (each with its own sources, or, with none, how
  strong it is: strong, moderate, weak). A claim the agent infers from its sources, rather than
  reads in them, says so.
- **Inside and outside.** Research looks inside (the machines and systems the agent works in:
  code, documents, data, logs, pictures and video on its machine and others, and kiln's own records:
  every agent's conversations through `conversations_search`, plans, CI runs, deployments) and
  outside (the web, standards and official docs, papers through arXiv, Semantic Scholar, Crossref
  or PubMed, books, news, blogs, forums). A plan's research is ready only with sources of both, or
  saying why there are none of one (`skipped`).

| Inside | Taken at its word | Outside | Taken at its word |
|---|---|---|---|
| a measurement (a command it ran, what it printed) | 0.95 | a standard or specification | 0.9 |
| code | 0.9 | a peer-reviewed paper | 0.85 |
| data | 0.8 | a product's own docs | 0.85 |
| logs, traces, metrics | 0.8 | a book | 0.75 |
| a document (README, design, notes) | 0.7 | a preprint, news | 0.6 |
| kiln's records | 0.65 | a blog, a wiki | 0.5 |
| pictures, video, sound | 0.6 | a forum, Q&A, issue | 0.4 |
| | | a vendor on its own product | 0.35 |
| | | any other web page | 0.3 |
| | | social media | 0.25 |

A source the agent says is out of date counts three quarters of that.

## Checks

kiln looks for each quote where its citation says it is, so a citation counts as far as it holds up
rather than as far as the agent says:

- a file, or a page on the web, is read on the agent's own machine (kiln asks it, as it asks about
  its folder), where the agent read it: kiln reaches nothing the agent couldn't, and only public
  pages (each redirect checked again);
- what a measurement printed is looked for in the agent's own conversation, among its tools'
  output; a record of kiln's in kiln (a plan's words, an agent's conversation).

A quote is found when its words are there in order, however they're spaced, in any case, with curly
quotes and dashes as plain ones (`…` or `...` stands for words left out). What can't be read (a PDF,
a page its scripts draw, one behind a sign-in, a file on another machine, a machine whose kiln is
older) is left unchecked and says why. Checks run in the background: research shows its quotes
being checked, then how each came out.

| A citation | Counts |
|---|---|
| its quote found where it says | 1.0 |
| its quote and where in the source, not checked | 0.75 |
| its quote, not where in the source, not checked | 0.65 |
| there (a picture, a file that isn't text) | 0.7 |
| a link or a path only | 0.5 |
| nothing to follow | 0.2 |
| looked for where it says, and not there | 0.3 |
| a dead link, a missing file | 0.1 |

## Confidence

kiln grades each finding the same way every time; the agent describes its evidence, never the grade.

- Each citation lends what its source's kind is taken at, times what the citation counts.
- Sources of one origin (one site, one file, one work, or one `origin` the agent names) count once,
  the strongest of them; independent ones add up: support is one minus what each origin leaves
  unsupported.
- A claim the agent infers counts 0.8 of that.
- Counterclaims weigh the same way by their own sources (with none: strong 0.6, moderate 0.35,
  weak 0.15), together one minus what each leaves.
- Its score is support × (1 − 0.8 × what weighs against it), out of 100.

| Grade | Score | Confidence |
|---|---|---|
| A | 85 and up | high |
| B | 70 to 84 | good |
| C | 50 to 69 | moderate |
| D | 30 to 49 | low |
| F | below 30 | very low |

Each finding shows why: how many independent sources and of what kinds, how its quotes held up, and
what's against it ("2 independent sources (code, a standard); 2 of 2 quotes found where cited").
One official doc quoted and found is an A (85); the same quote not yet checked a C (64); a lone
blog post linked without a quote an F (25).

## Presenting it

`research_get` answers with `markdown`: each finding with its grade, score and why, its quotes and
how kiln's look for each came out, its counterclaims, then its sources, numbered. Agents present
research that way whenever they present it (in a conversation, in a plan): every claim with its
sources and its grade, never an unweighed one. A plan's page shows its research under its steps,
live while it's planned; research has a page of its own (`/agents/research/<id>`).

## Tools and API

Agents record research with `research_start`, `research_add` and `research_get` (`research_list` too),
and look through kiln's own conversations with `conversations_search`; in code mode,
`kiln.research.*` and `kiln.conversations.search`.

| Method | Path | |
|---|---|---|
| GET | `/api/research` | research, newest change first (`plan`, `agent`) |
| POST | `/api/research` | `{question, plan?}`: research begun (an agent's own, when an agent asks) |
| GET | `/api/research/:id` | its sources and findings, each graded (`confidence: {score, grade, words, support, against, reliability, citation, independent, cites, found, failed, why}`), its summary, and `markdown` |
| POST | `/api/research/:id/findings` | `{sources?, findings?, remove?, skipped?}`: added or changed by id; a cite's `source` is a source's id or the source itself |
| GET | `/api/harness/conversations/search` | `q`, `agent`, `kind`, `tool`, `since`, `until`, `limit`: events of agents' conversations, newest first |

An agent adds to its own research and that of the agents it started; a person to any.
