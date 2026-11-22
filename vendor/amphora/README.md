# amphora
Agentic database for durable, transactional records - from a laptop to global multi-region.

Amphora is the database kiln keeps every record in: KilnDB made a product. It keeps records, never bytes: kiln's sessions, conversations, goals, plans, deploys, CI, runners, places,
repos, sync, settings, users, grants and audit, and the records of Hex, Veil, Reliquary and Quiver's
catalog. One binary runs from a laptop to a fleet to global multi-region: Turso, vendored and made
multi-writer, is each group's state machine, on a write-ahead log that is never optional. Bytes go to
Mantle.

**Status:** Amphora is a library today: a program opens a database in its own process
(`amphora::database::Database`) and a second process on the same machine reaches it through the
owner's local protocol ([docs/design/client-protocol.md](docs/design/client-protocol.md)). The
`amphora` binary says what it is, and nothing more yet. `STATUS.md` says what works and what's next.

## From a clone to a working Amphora

These steps take a fresh machine (Linux, macOS or Windows; x86_64 or aarch64) to a built and tested
Amphora. Each says what you see when it worked.

### 1. Prerequisites

- **git**, with access to `github.com/hyper-light/amphora`.
- **rustup** (https://rustup.rs). Don't install a toolchain by hand: `rust-toolchain.toml` pins the
  exact one (its version, `rustfmt`, `clippy`, and the macOS and Windows targets the gates lint),
  and the first `cargo` command in the clone installs it.
- **A C compiler and linker**: `cc` or `clang` on Linux (`build-essential` on Debian and Ubuntu),
  the Xcode command-line tools on macOS (`xcode-select --install`), the Visual Studio Build Tools
  on Windows. Some dependencies build C code.
- **python3**, for the vendoring check (`scripts/vendor.py`).
- **bash**, for `scripts/` (Git Bash on Windows).
- Disk: a debug build and its tests take several GB under `target/`. Put `CARGO_TARGET_DIR`
  somewhere with room if the clone's disk is small.

### 2. Clone

```sh
git clone git@github.com:hyper-light/amphora.git
cd amphora
```

Everything Amphora builds from is in the clone: its engine (`crates/engine*`) and what it vendors
(`vendor/`, patched in through `Cargo.toml`'s `[patch.crates-io]`). Nothing else to fetch by hand.

### 3. Build

```sh
cargo build --release      # target/release/amphora
target/release/amphora --version
```

Worked when it prints `amphora 0.1.0 (unknown commit)` (kiln CI sets the commit).

### 4. A working Amphora

```sh
mkdir -p ~/.cache/amphora-try
cargo run -p amphora --example quickstart -- ~/.cache/amphora-try/notes.db
```

[`crates/amphora/examples/quickstart.rs`](crates/amphora/examples/quickstart.rs) opens a database
file, gives it a table, writes a row and reads the rows back in one atomic batch. Worked when it
prints `[Integer(1), Text("hello from Amphora")]`; each run adds a row. That file is the shortest
correct use of the API: start from it.

### 5. Test

The tests of what you change first, then the gates.

```sh
cargo test -p amphora --locked                       # the amphora crate
cargo test -p amphora --locked --test design_index   # docs/design.md against the records
bash scripts/gates.sh                                # every gate, as kiln CI runs them
```

`scripts/gates.sh` runs, in order, and stops at the first failure: `cargo fmt --all --check`; clippy
on the workspace for this machine, for `aarch64-apple-darwin` and for `x86_64-pc-windows-msvc`;
`scripts/check-production.sh` (no panics in production code, CLAUDE.md §1); `cargo test --workspace
--locked`; and `python3 scripts/vendor.py check`. Worked when it exits 0. The whole workspace's
tests are heavy: on a shared machine run them with `nice -n 10` and your own `CARGO_TARGET_DIR`.

## Where things are

| Path | What |
|---|---|
| `CLAUDE.md` | The rules every change follows: no panics, bounds, typed refusals, cited decisions, the gates. Read it before changing anything. |
| `crates/amphora` | The database: `database` (open, SQL batches, Records, migrations), `store` (the commit pipeline), `groups`, `reshard`, `replication`, `transport` (the local owner protocol), and the `amphora` binary. |
| `crates/api` | `amphora-api`: the traits others reach Amphora through (`Records`), its simulator and conformance suite. |
| `crates/engine*` | Amphora's engine: Turso's four crates as of 3e09a82, Amphora's own since (`crates/engine/ORIGIN.md`). |
| `crates/columnar`, `crates/corpus` | Columns for analytics; the query corpus the benchmarks run. |
| `docs/design.md` | Every T1 decision, one line each, linking its record; the client protocol compared with PostgreSQL's, MySQL's, SQLite's and Hrana. |
| `docs/design/` | The decision records, an area a file, and the target (`docs/design/README.md`). |
| `docs/research/` | The sources the decisions cite. |
| `docs/requirements.toml` | The requirement IDs (`CP-T1.n`, `AC-T1.n.k`, `T-T1.n.k`) code, tests and commits cite. |
| `STATUS.md` | What works today, and what closes each open piece. |
| `bench/` | Benchmarks against the named competitors, and their records. |
| `scripts/` | The gates (`gates.sh`, `check-production.sh`) and the vendoring tool (`vendor.py`). |
| `vendor/` | What Amphora vendors, each at one revision (`vendor/MANIFEST.toml`). |

MIT licensed (`LICENSE`).
