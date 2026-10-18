# Argo CD's health checks for custom resources

The Lua health checks of Argo CD's resource customizations library, with their tests, copied
from <https://github.com/argoproj/argo-cd/tree/4e68298/resource_customizations> under the Apache
License 2.0 (in `LICENSE` here). kiln runs them as Argo CD does (`crates/kiln-cd/src/lua.rs`), so
a custom resource reads the same health in kiln as in Argo CD, and kiln's tests run Argo CD's own
expectations (each kind's `health_test.yaml` and its `testdata`) against them.

Only `health.lua` is built into kiln. To update, copy each kind's `health.lua`,
`health_test.yaml` and the test data it names from a newer Argo CD.
