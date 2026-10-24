//! T-T2.1.2: the write-ahead log is mandatory (docs/design/focal-bus.md, "GroupStore mandated").
//! Do build a program that names hyper-durable's `RamStore` outside a test, expect the build to
//! fail; do build the same program naming `GroupStore` over a real device, expect it to build.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

#[test]
fn ram_store_outside_a_test_fails_the_build() {
    let builds = trybuild::TestCases::new();
    builds.pass("tests/build/group_store.rs");
    builds.compile_fail("tests/build/ram_store.rs");
}
