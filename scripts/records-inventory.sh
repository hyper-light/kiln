#!/bin/sh
# Writes docs/records-inventory.md from the code (CP-T1.5, G9 step 2): every SQLite table and JSON
# record kiln keeps. crates/kiln-seams/tests/records_inventory.rs holds the file to the code.
set -eu
cd "$(dirname "$0")/.."
KILN_BLESS=1 cargo test -p kiln-seams --test records_inventory -- cp_t1_5_every_table_and_json_record_has_its_inventory_entry
