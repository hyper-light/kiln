# Quiver: rules for every change

Quiver is the topic store: the cornerstone for an event-driven world (github.com/hyper-light/quiver; track T2 of kiln's critical path, kiln plan
`01a157fdefc37396957ea10adb51a19f`). Topics, partitions, placement, replication, consumer groups and SQS-style queues, built on Focal's
bus with its gaps fixed and offered back. One binary from a laptop to global multi-region. Kiln
moves to publish and subscribe on it.

These rules are absolute. They are kiln's critical-path bar (its master plan §3, kiln plan
`01a157fddb8276a2996246ea931c4808`), the bar Mantle, Focal, Slates and Shards hold. Production code is everything compiled
outside `#[cfg(test)]`, `tests/`, `benches/` and `examples/`.

## 1. Production code never panics

No `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, `unreachable!`, `assert!`, slice indexing
or unchecked arithmetic. Every failure is a typed error returned to a caller that handles it:
`checked_*` arithmetic (`saturating_*` only where saturation is the stated meaning), `try_from` for
narrowing, `.get(..)` for indexing; a poisoned lock, a closed channel or an ended task is an error to
return. The workspace lints (`Cargo.toml`, `clippy.toml`) encode this, and `scripts/check-production.sh`
is its gate: it forbids them, so an `#[allow]` or `#[expect]` for one in production code fails it. Test code opts out at its crate root
with `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::arithmetic_side_effects, clippy::disallowed_macros))]`.
Vendored code Quiver changes follows the same rule.

## 2. Every resource has a bound

Every queue, cache, map, buffer, retry loop and wait has a stated bound and a stated overflow
behavior: a typed refusal, or an eviction by a stated rule. A loop over user-scaled data ends on a
counted budget or a deadline. Memory and disk are charged to budgets.

## 3. Typed refusals, never silent drops

What can't be done is refused with a typed error that says what and why. Nothing is dropped
silently: not an update, a message, a byte or a request.

## 4. Root causes

A bug fix starts with a failing test. A failing test is fixed at its cause, never by a longer
timeout, a retry or a raised limit. A test waits on the fact it needs, never on a wall-clock guess.

## 5. Decisions cite evidence

Each design decision is in `docs/design/` with its sources (peer-reviewed work and primary sources:
specifications, RFCs, kernel and vendor documentation), its evidence, and what would falsify it;
source notes go in `docs/research/`. A tuning constant is measured on the running hardware or cited,
and its comment says which.

## 6. Tests exercise behavior

Each test reads "do X, expect Y" and cites the requirement it serves (`AC-T2.n.k`, `T-T2.n.k`,
docs/requirements.toml). The kinds that apply: model-based and oracle tests, non-vacuity counters,
deterministic simulation with a nemesis, crash injection at every write, loom, Miri, shuttle and TSan,
hostile input to every parser, Elle-style isolation checks, and doc-truth tests.

## 7. Benchmarks are recorded

A performance claim names the run that measured it: a release binary with its hardware, commit, date
and load (load average under 2, nothing else building), best of N, recorded in `bench/`. Comparisons
are class for class, against named competitors on the same hardware.

## 8. One code path

A laptop, a fleet and global multi-region run the same code, and the same differential suite passes on
one node, three nodes and three regions. Each step up in scale adds only the choice it can't avoid.

## 9. Seams

Other products are reached only through their `-api` crates, and Quiver only through its own: each
trait with a simulator implementation and a conformance suite both pass. New subsystems are pure cores
(sans-IO) driven by shells.

## 10. Observability is part of done

Metrics are `quiver_*` in base units (`_total`, `_seconds`, `_bytes`) with no high-cardinality labels;
events and spans go on dotted targets with an `event` field. New names are agreed with kiln's Observe
owner (the Kiln Metrics agent). Every component ships its dashboard rows.

## 11. No runtime recursion

No function calls itself, directly or through others, and no algorithm walks a user-scaled structure
depth-first on the call stack. Work is flat: loops with counted budgets and bounded queues.

## 12. Portable

Linux, macOS and Windows, on x86_64 and aarch64. Platform code sits behind `cfg` with a portable path
beside it, or a typed refusal naming the platform. `unsafe` is denied; where an OS interface needs it,
its file is named here with the reason, and every block carries a `// SAFETY:` comment stating its
invariant.

## 13. Lean code

Code states what the system does now: no speculative abstraction, no dead or commented-out code.
Comments explain why.

## Requirement IDs

`docs/requirements.toml` is the registry: the track's goals (`CP-T2.n`), their criteria
(`AC-T2.n.k`) and tests (`T-T2.n.k`), from kiln's plan, with Quiver's own IDs beside them as
its design names them. Code comments, commits, tests and benchmarks cite them.

## Vendoring

`vendor/MANIFEST.toml` names what Quiver vendors, each at one revision. `scripts/vendor.py` (kiln's
copy, unchanged) takes a project at a revision with its recorded changes re-applied
(`vendor/patches/<project>/`, each a row of the project's `UPSTREAM.md`), and `check` proves each
vendored tree is its upstream plus those changes. Another hyper-light product is a dependency by name
and version (`amphora-api = "0.1"`), patched to its vendored copy in `Cargo.toml`'s
`[patch.crates-io]`; kiln patches the same names to its own copies, so everything builds against one
revision of each (kiln's docs/vendoring.md). No package takes the name of a crate kiln already links:
`hex` is crates.io's hex encoding.

## Gates

Every commit passes these on its final tree. `bash scripts/gates.sh` runs them in order and stops at
the first failure; kiln CI runs it on every push and pull request (`.kiln/ci.yaml`).

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo-zigbuild clippy --workspace --all-targets --locked --target aarch64-apple-darwin -- -D warnings
cargo-zigbuild clippy --workspace --all-targets --locked --target x86_64-pc-windows-gnu -- -D warnings
bash scripts/check-production.sh
cargo test --workspace --locked
python3 scripts/vendor.py check
```

The macOS and Windows lints compile aws-lc's C (Focal's bus takes it through hyper-raft's TLS and
QUIC), so zig is their C compiler, through cargo-zigbuild; `gates.sh` takes them from the PATH or
installs kiln's pinned versions into a virtualenv of their own.

## Working here

The person's standing rules (kiln's master plan §8):
- Merging to main, pushing anywhere, deploying, and creating repositories take the person's word.
- Never print a secret or a token; a secret typed into a sign-in is never logged or kept.
- Stop only the processes you started, by the PIDs you recorded; never pick them by command line.
- Guard every `rm` (`"${T:?}"`). Labs and big files go under `~/.cache`, never `/tmp`.
- Build with `nice -n 10` and your own `CARGO_TARGET_DIR`. Heavy builds and benchmarks go to fedora or
  jupiter: adacachy runs production. A benchmark takes its slot on kiln's bench calendar.
- No bare `git stash`; rebase and squash only onto a fixed SHA; gate every push on every check's exit
  code; wire formats change append-only.
- Built, merged and live are three different things: say which.

## Where things are

- `docs/design/`: the design and its decision records.
- `docs/research/`: the source notes decisions cite.
- `docs/requirements.toml`: the requirement registry.
- `STATUS.md`: what works today, and what closes each open piece.
- `bench/`: benchmarks and their records.
- `vendor/`: what Quiver vendors (`vendor/MANIFEST.toml`).
