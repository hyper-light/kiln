#!/usr/bin/env python3
"""Plan step 27, step 2 (kiln plan 01a1ce705e5c7504b88732f443d99288): each of the unpack module's guards
this step adds, removed one at a time. Each mutant must make the test named beside it fail. The module
is rewritten with its own text after each (a new mtime, so cargo rebuilds it rather than keep the
mutant), and the module's whole suite runs once more at the end, unmutated.

Run on a runner from the repository root, as run-tests.sh sets cargo:
    python3 -I docs/research/s27-cursor-agentfile/step2/mutate.py [name ...]
"""

import os
import subprocess
import sys

SRC = "crates/kiln/src/agentfiles/unpack.rs"

# (name, the test that must fail, the text removed, the text put in its place)
MUTANTS = [
    ("staging-is-the-target", "the_per_arch_directory_appears_only_once_the_whole_archive_is_in",
     "    let staging = staging_for(&unit)?;", "    let staging = unit.clone();"),
    ("entries-bound-removed", "nothing_of_a_tree_shows_until_all_of_it_is_in",
     "        if entries > tree.entries {", "        if false && entries > tree.entries {"),
    ("bytes-bound-removed", "nothing_of_a_tree_shows_until_all_of_it_is_in",
     "                if bytes > tree.unpacked_bytes {", "                if false && bytes > tree.unpacked_bytes {"),
    ("entries-total-removed", "nothing_of_a_tree_shows_until_all_of_it_is_in",
     "    if entries != tree.entries {", "    if false && entries != tree.entries {"),
    ("bytes-total-removed", "nothing_of_a_tree_shows_until_all_of_it_is_in",
     "    if bytes != tree.unpacked_bytes {", "    if false && bytes != tree.unpacked_bytes {"),
    ("sticky-allowed", "each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing",
     "    if mode & 0o1000 != 0 {", "    if false && mode & 0o1000 != 0 {"),
    ("prefix-unchecked", "each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing",
     "        if !self.lead.iter().zip(&lead).all(|(a, b)| a == b) {",
     "        if false && !self.lead.iter().zip(&lead).all(|(a, b)| a == b) {"),
    ("duplicates-allowed", "each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing",
     "        if !self.paths.insert(path.to_path_buf()) {", "        if !self.paths.insert(path.to_path_buf()) && false {"),
    ("harness-file-allowed", "each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing",
     "    if root && rel.to_str()", "    if false && root && rel.to_str()"),
    ("nameless-file-dropped", "each_entry_a_tree_may_not_hold_is_refused_naming_it_and_leaves_nothing",
     "        return Err(Why::Nameless);", "        Goes::Lead { mode: None }"),
    ("modes-as-archived", "a_strip_1_tree_appears_whole_with_its_files_directories_and_modes",
     "    if mode & 0o100 != 0 { 0o755 } else { 0o644 }", "    mode & 0o777"),
    ("target-there-unchecked", "a_target_already_there_is_refused_and_left_as_it_was",
     "                None => return Err((None, Why::Exists(as_shown(at)))),", "                None => return Ok((unit, PathBuf::new())),"),
    ("neither-taken", "the_unpack_object_is_one_form_or_refused",
     "            (false, false) => Err(Why::Neither),",
     "            (false, false) => Ok(Spec::Tree(Tree { strip: 0, unpacked_bytes: 0, entries: 0 })),"),
    ("discard-unsearched", "a_directory_its_owner_may_not_search_is_kept_and_still_discarded_whole",
     "    let mut todo = vec![staging.to_path_buf()];", "    let mut todo: Vec<PathBuf> = Vec::new();"),
]


def cargo_test(test):
    env = dict(os.environ)
    env["PATH"] = os.path.expanduser("~/.cargo/bin") + os.pathsep + env.get("PATH", "")
    env["CARGO_TARGET_DIR"] = os.path.expanduser("~/.cache/s27-unpack/target")
    env.setdefault("CARGO_BUILD_JOBS", "4")
    env["CARGO_PROFILE_DEV_DEBUG"] = "line-tables-only"
    env["KILN_UI_SERVER"] = "off"
    cmd = ["nice", "-n", "10", "cargo", "test", "--locked", "-p", "kiln", "--lib", test]
    run = subprocess.run(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return run.returncode, run.stdout


def main(names):
    with open(SRC, encoding="utf-8") as f:
        original = f.read()
    chosen = [m for m in MUTANTS if not names or m[0] in names]
    results = []
    try:
        for name, test, old, new in chosen:
            if original.count(old) != 1:
                results.append((name, test, "NOT-APPLIED", f"{original.count(old)} matches"))
                continue
            with open(SRC, "w", encoding="utf-8") as f:
                f.write(original.replace(old, new))
            code, out = cargo_test(test)
            failed = f"agentfiles::unpack::tests::{test} ... FAILED" in out
            built = "test result:" in out
            verdict = "KILLED" if code != 0 and failed else ("NOT-BUILT" if not built else "SURVIVED")
            tail = [l for l in out.splitlines() if "panicked" in l or "assertion" in l or l.startswith("error")][:3]
            results.append((name, test, verdict, " | ".join(tail)))
            print(f"{name}: {verdict} (exit {code})", flush=True)
            if verdict != "KILLED":
                print(out[-6000:], flush=True)
    finally:
        with open(SRC, "w", encoding="utf-8") as f:
            f.write(original)
    print("\n== mutants")
    for name, test, verdict, why in results:
        print(f"{verdict:12} {name:24} {test}  {why}")
    code, out = cargo_test("agentfiles::unpack")
    print("\n== unmutated: exit", code)
    print("\n".join(l for l in out.splitlines() if l.startswith("test ") or l.startswith("test result:")))
    killed = sum(1 for r in results if r[2] == "KILLED")
    print(f"\n== {killed} of {len(results)} mutants killed; unmutated suite exit {code}")
    return 0 if killed == len(results) and code == 0 else 1


if __name__ == "__main__":
    sys.exit(main(set(sys.argv[1:])))
