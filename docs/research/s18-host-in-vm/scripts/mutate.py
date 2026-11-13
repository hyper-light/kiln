# Applies step 18's guard-removal mutants to a kiln checkout (argv[1]) and prints each mutant's id and
# the test that must fail for it. Each replacement must match exactly once.
import sys, os
root = sys.argv[1]
M = [
 ("M1", "crates/kiln/src/api.rs",
  "        i.person = caller.and_then(crate::harness::lineage::person_of);",
  "        let _ = caller; // MUTANT M1",
  "api::tests::step18_an_isolated_agents_person_is_whoever_asks"),
 ("M2", "crates/kiln/src/harness/isolated/home.rs",
  "    if id.is_empty() || !id.bytes().all(portable_byte) || id.starts_with('.') {",
  "    if id.is_empty() { // MUTANT M2",
  "harness::isolated::home::tests::step18_each_persons_and_harness_volume_is_its_own"),
 ("M3", "crates/kiln/src/harness/isolated/home.rs",
  "        if held.is_empty() {",
  "        if held.is_empty() && root.as_os_str().is_empty() { // MUTANT M3",
  "harness::isolated::home::tests::step18_volumes_every_machine_can_keep_or_a_refusal"),
 ("M4", "crates/kiln/src/harness/homes.rs",
  "        if spec.isolation.is_some() { Self::microvm(&spec.env) } else { Some(Self::here(&spec.env)) }",
  "        Some(Self::here(&spec.env)) // MUTANT M4",
  "harness::isolated::tests::step18_an_isolated_session_without_its_volumes_view_reads_nothing_here"),
 ("M5", "crates/kiln/src/harness/isolated/env.rs",
  "    (\"KILN_HARNESS_SOCK\", Fate::LeftOut(",
  "    (\"KILN_HARNESS_SOCK_MUTANT_M5\", Fate::LeftOut(",
  "harness::isolated::env::tests::step18_every_kiln_variable_a_harness_gets_is_decided_for_a_microvm"),
 ("M6", "crates/kiln/src/harness/isolated/mcp.rs",
  "    if destinations.iter().any(|d| d.trim().eq_ignore_ascii_case(&want)) {",
  "    if destinations.iter().any(|d| d.trim().eq_ignore_ascii_case(&want)) || port > 0 { // MUTANT M6",
  "harness::isolated::mcp::tests::step18_mcp_goes_only_where_the_image_grants_it"),
 ("M7", "crates/kiln/src/harness/isolated/host_tools.rs",
  "    if tools.is_empty() { Ok(()) } else { Err(Refusal::HostTools(tools)) }",
  "    let _ = tools; Ok(()) // MUTANT M7",
  "harness::isolated::host_tools::tests::step18_a_session_that_needs_host_tools_is_refused_isolation_saying_why"),
 ("M8", "crates/kiln/src/harness/trouble/mod.rs",
  "            if category.is_some_and(|c| CLAUDE_AUTH.contains(&c)) || status.is_some_and(refused) {",
  "            if category.is_some_and(|c| c == \"MUTANT M8\") {",
  "harness::trouble::tests::step18_h6_claude_says_its_401_at_its_first_retry"),
 ("M9", "crates/kiln/src/harness/trouble/mod.rs",
  "                if status.is_some_and(refused) {",
  "                if status.is_some_and(|s| s == 0) { // MUTANT M9",
  "harness::trouble::tests::step18_h6_codex_says_its_401_at_its_first_retry"),
 ("M10", "crates/kiln/src/harness/isolated/home.rs",
  "        Some(under(&self.base.join(SCRATCH), guest))",
  "        Some(PathBuf::from(guest)) // MUTANT M10",
  "harness::isolated::home::tests::step18_a_harness_home_is_its_persons_volume_and_read_here"),
 ("M11", "crates/kiln/src/harness/lineage.rs",
  "        i.person = person.map(str::to_owned).or(i.person.take());",
  "        let _ = person; // MUTANT M11",
  "harness::lineage::tests::step18_an_isolated_agents_person_is_its_askers_else_its_parents"),
 ("M12", "crates/kiln/src/harness/trouble/mod.rs",
  "            let Some(was) = core.state.trouble.take() else { return };",
  "            let Some(was) = core.state.trouble.clone() else { return }; // MUTANT M12",
  "harness::trouble::tests::step18_h6_an_answer_ends_it_and_another_kind_is_said_anew"),
]
for mid, f, a, b, t in M:
    p = os.path.join(root, f)
    s = open(p).read()
    n = s.count(a)
    if n != 1:
        sys.exit(f"{mid}: {f} matches {n} times")
    open(p, "w").write(s.replace(a, b, 1))
    print(mid, t)
