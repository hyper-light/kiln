# Plan fixtures (docs/ci.md Appendix F, F-10 to F-15)

Each directory is part of a repository: `tests/plan_fixtures.rs` pushes them all to one and checks
W04-AC07 on each (the plan preview's legs, decided conditions and removed combinations are the
legs its run creates), and that each run succeeds.

| Fixture | What | Checks |
| --- | --- | --- |
| `f10-cross-arch` | a Rust binary built for `linux/amd64`, `linux/arm64` and `darwin/arm64`, `matrix: same` tests, one job collecting the three | W04-AC07, AC08 |
| `f11-timing-shards` | forty test files sharded by recorded times into legs of about five minutes; the test makes `tests/` | W04-AC05, S04 |
| `f12-matrices` | GitHub's documented matrix examples; the test lists the legs GitHub documents | W04-AC01 |
| `f13-dynamic` | a matrix from a `plan` job's output, and a job decided before anything runs | W04-S02 |
| `f14-reusable` | reusable workflows three levels deep: typed inputs, `secrets: inherit`, outputs | W04-AC04 |
| `f15-outputs` | typed, plain and secret outputs, `exports` / `env-from`, `run.env`, `legs` | W04-AC06, S05 |

F-10 is W04-AC08 too, on runners of those three platforms: it builds with `rustc`, and its test
runs each binary where it was built for.
