# amphora
Agentic database for durable, transactional records - from a laptop to global multi-region.

Amphora is the database kiln keeps every record in: KilnDB made a product. It keeps records, never bytes: kiln's sessions, conversations, goals, plans, deploys, CI, runners, places,
repos, sync, settings, users, grants and audit, and the records of Hex, Veil, Reliquary and Quiver's
catalog. One binary runs from a laptop to a fleet to global multi-region: Turso, vendored and made
multi-writer, is each group's state machine, on a write-ahead log that is never optional. Bytes go to
Mantle.

**Status:** the repository has its bar (`CLAUDE.md`, the gate, the requirement registry); the
`amphora` binary says what it is, and nothing more yet. `STATUS.md` says what works and what's next;
`docs/design/` is the design.

```sh
cargo build --release      # target/release/amphora
bash scripts/gates.sh      # every gate, as kiln CI runs them
```

MIT licensed (`LICENSE`).
