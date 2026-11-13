# A conversation that ran in a microVM, picked up again in one from the UI

kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 35; plan 01a1d2c16eb074f19cf7ae7f1025c0be step 5. The design,
decided before the code (R02), from kiln main 8d36e0d35.

## What held before

- The composer's past sessions come from its machine's history (`aisessions::Scanner::history`), which read
  Claude Code's, Codex's and Devin's own folders on the machine alone (`CLAUDE_CONFIG_DIR` or `~/.claude`,
  `CODEX_HOME` or `~/.codex`, Devin's `devin/cli`), never a person's microVM volume (research f4).
- An isolated session's harness keeps its configuration, sign-in and transcripts in its person's volume,
  `<kiln's state>/homes/<person>/<harness>/`, each of its image's harness volumes beneath it at its own path
  (`harness::isolated::home::Layout`, step 18). kiln reads it there through `Layout::host_env`.
- The composer's pick-up (`views/harness/new-agent.tsx`, `resume`) sent `spec.resume` and no `isolation`,
  so a conversation that ran in a microVM would go on as a process on the machine (f3), whose own harness
  home doesn't hold it.
- An ended agent's Restart goes on from `Session::resume_spec`, a clone of its spec (its `isolation` with
  it) with the conversation to resume: it kept its isolation, but no test held it.
- The machine history lists Claude Code's, Codex's and Devin's alone: the other seven harnesses'
  conversations are in no machine history, on the host or in a volume (gaps.json's `ui_pick_up` rows).

## Decisions

1. **Whose volume a history reads: the asker's alone.** The person is kiln's to say, as an isolated start's
   `Isolation::person` is (`api.rs` `as_asked`, `harness::lineage::person_of`): whoever asks, never what's
   asked; on a kiln nobody else signs in to, `owner`. A person's volume is theirs alone (step 18), so no
   other person's conversations are listed to them, and none could go on in a microVM of theirs.
   - Rejected: the runner lists every person's volumes and the server keeps the asker's. The runner's
     `limit` would be shared out among people (a person with many newer conversations hides another's),
     the runner would read and send every person's transcripts on each ask, and a server from before this
     would list them all as the machine's own, to be resumed as processes.
2. **On the wire: an ask that names the person.** `ServerMsg::HarnessHistoryFor` (appended last, its ask
   as JSON: `{kind, limit, person}`), sent only to a runner that names the feature
   `harness.history-volumes` (docs/wire.md, "Named features"); its answer is `AgentMsg::HarnessHistory`, as
   before. A runner from before is asked as before and reads no volume; a server from before never sends the
   new ask, so no runner lists a volume's conversations to a UI that would resume them as processes.
3. **Where in a volume: as kiln's Agentfile for its harness lays it out.** `home::Layout` from the
   built-in's harness volumes and run environment (`home::of_builtin`), then `host_env`'s
   `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `XDG_DATA_HOME` (Devin's `devin/cli` below it), read with the same
   readers as the machine's own folders. A person's own Agentfile is built beside kiln's harness directory
   (`sandbox::image`), so one made from kiln's keeps that layout. One that keeps its harness's configuration
   at other paths isn't read: only its launch knows its image's volumes (owed, below).
   - What lives in a volume is a microVM's, so the machine's own signs of a live session don't apply
     there: Claude Code's session records name guest pids, and no process here is its harness. A volume's
     conversation is left out only where kiln's own sessions hold it (`running`), as on the machine.
4. **Each says how it ran.** `PastSession::isolation`, the typed `Isolation` (R04): `{kind: "microvm",
   person}` for a volume's conversation, absent for the machine's own. A new optional JSON field: older
   readers skip it.
5. **The image it goes on in: the Agentfile kept for its harness in its folder, else kiln's.** The
   composer keeps an Agentfile per harness and per project, the folder an agent starts in
   (`views/agentfiles/choose.ts`, `chosenFor`); the pick-up starts the conversation in its own folder, so
   it takes the one kept for its harness there, else kiln's Agentfile for its harness (`harness-<kind>`).
   Never none: a conversation in a volume goes on in a microVM, never as a process on the machine. The
   server checks the image as for any start (`lineage::image_for`: kiln's for its harness, or one of the
   asker's own) and sets the person from the asker.
   - Rejected: the composer's current Agentfile row. It is the choice for the composer's folder, which
     needn't be the conversation's.
   - Not built: a record of the image each conversation ran in. Nothing could write it yet: no launch
     prepares a person's volume today (`sandbox::microvm()` holds none; step 17's launch slice calls
     `Layout::prepare`), and Codex's and Devin's conversation ids come after their harness's first
     answers, not at launch. Owed with the launch (below).
6. **Restart keeps its isolation.** A test holds that an ended isolated agent's Restart goes on in a
   microVM of its own, from its image, in its own conversation, and starts no process on the machine.

## R01, R03, R04

- R01: no new panic path; every read of a volume is a missing file or an empty list at worst.
- R03: no new number. The history's `limit` and `HISTORY_MAX` are as they were; `homes` is a name.
- R04: `PastSession::isolation` is typed; a person kiln keeps no volume for is `home::Refusal::Person`,
  logged, and the machine's own conversations are listed without theirs.

## Tests

- `aisessions::tests::s35_history_lists_a_persons_volume_conversations_marked_isolated`: Claude Code's,
  Codex's and Devin's conversations in a fixture volume, listed for their person alone and marked isolated;
  another person's, and kiln's own sessions' conversations, not listed.
- `proto::tests::wire_registry_holds_every_bit_and_variant_once` and `wire::tests`: the new variant at its
  index, its row and class in docs/wire.md, its feature named.
- `hub` and `api`: the ask names the asker (`harness::registry` local path, `HarnessHistoryFor` to a runner
  naming the feature, `HarnessHistory` to one that doesn't).
- `harness::host::tests::s35_an_ended_isolated_agents_restart_keeps_its_isolation`.
- UI: `tests/pick-up.test.ts` (the spec a pick-up sends) and `e2e/pick-up-microvm.spec.ts` against the mock
  (a Codex conversation in a person's volume picked up from the composer's past sessions: the
  `POST /api/harness/sessions` body has `spec.resume` and `spec.isolation {microvm, image}`), Chromium and
  WebKit.

## Owed

- To step 17's launch, as it prepares a person's volume: keep the layout it prepared and the image it ran,
  beside the volume, so the history reads a person's own Agentfile's layout and names the image a
  conversation ran in.
- To ROOT: the real-VM run (step 7): an isolated conversation ended, then picked up again from the composer
  on a runner that starts microVMs (`ui/e2e/vm-harnesses.spec.ts`, against ROOT's server).
