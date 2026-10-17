# F-02: the v1 pipeline corpus (docs/ci.md Appendix F)

Every `.kiln/*.yaml` of hyper-light's public repositories on 2026-10-17, copied verbatim, for the v1
regression tests (W01-AC09, W02-AC04). Each directory is a repository; the commit it came from:

| Repository | Commit |
| --- | --- |
| `hyper-light/hyperscale` | `9e7fcdadf4d7b29e5d161060ec68b8b70b2fd30f` |
| `hyper-light/mkfst-py` | `c44c36e76ff0b1cc622cfca14604ef874ce5f519` |
| `hyper-light/cocoa` | `c3aef4bec4b554ea8593dde8e6d2c2bb08efeb5b` |
| `hyper-light/sylk` | `6d4b414dfa0ac41a4ae50799eb47fcc2adcd48e5` |
| `hyper-light/vorpal` | `4ec68042122fa7915461211c3c82f1c8bd95d0d9` |
| `hyper-light/slates` | `b0baf0f2b7e9dd8d266069840de85959cfdf5221` |
| `hyper-light/focal` | `dd5f80a3d268009f3aee63dcf09b53a0da04c099` |
| `hyper-light/mantle` | `935428cdc576ba75fd8f7e56cb9f7a608f0dad15` |
| `hyper-light/hyperlight-site` | `76e6e5149162f72a389f62fae9891a1d3fd5e9cf` |
| `hyper-light/shards` | `9216e6db0d983f06f2188dc4a3025f22b1bda825` |
| `hyper-light/hyper-raft` | `e9b60d64758b83d966aaab535ee4d38de955da03` |
| `hyper-light/kiln` | `edc9ae80f45423d321bea255604a297f6c429570` |

Refresh with `crates/kiln-ci/tests/fixtures/v1/refresh.sh` (no API quota: a sparse clone per repository).
