# The executor: the work a plan's bytecode does

Amphora's engine runs a plan as bytecode (`crates/engine/vdbe`). Each decision here removes work the
bytecode repeated on every run of a statement, where Amphora's bench suite (`bench --suite`,
AC-T1.2.1) and its profiles (`AMPHORA_PROFILE`) found it, recorded with its sources, its evidence
and what would falsify it (CLAUDE.md §5).

## 1. A statement keeps its small ephemeral tables from one run to the next

**Decision.** When a statement is reset to run again, each ephemeral table it opened (an IN list's
keys, an IN subquery's rows, a materialized subquery's index) that holds at most 16 pages in its
pager's cache is kept with its temp file (`ProgramState::ephemeral_kept`), and the next run's
`OpenEphemeral` empties and reuses it, as a table opened twice in one run already was. A larger
table is dropped as before, and every kept one is dropped when the statement is prepared again or
the cursor closed.

**Why.** Each run built its ephemeral tables from nothing: a temp file, a pager with a cache of its
own, a read and a write transaction, page 1 and a B-tree, then dropped them all at the reset. A
profile of the suite's `in_list_10` (ten rows by primary key in an IN list, fedora, 1 client) found
building and dropping the IN list's table about a third of the statement's time: `op_open_ephemeral`
10%, `op_idx_insert` 9%, the reset dropping the temp file 4%, the transaction and page 1 about 8%.

**Sources.** SQLite keeps no ephemeral table between runs, but its are in-memory B-trees on the
connection's pager (`sqlite3BtreeOpen` with a NULL file name, `src/btree.c`), far cheaper to build
than a pager of their own; reusing the table here costs the same as SQLite's once it is built.

**Evidence.** `crates/engine/tests/ephemeral_reuse.rs`: a prepared IN list run four times with other
keys (overlapping, repeated, absent) returns each run's own keys once each; IN subqueries small
enough to keep and too large to keep, run in turns with other values, count each run's own rows.
With a kept table reused without being emptied (a mutant), both fail, the IN list's second run
returning the first run's keys too.

**Falsified by.** A run that reads a key of a run before; a statement holding more memory between
runs than 16 pages for each ephemeral table it opens.

## 2. `IN (SELECT ...)` answers a row with one search of its list

**Decision.** As a condition (`WHERE`, `ON`, `HAVING`), `x IN (SELECT ...)` is one search of the
subquery's ephemeral index: found keeps the row; a miss, or a NULL on the left, fails it. A
condition fails on NULL as on FALSE, so it never asks whether the list holds a NULL. Where the two
differ (`NOT IN`, or the answer used as a value), a one-column list reads its first entry alone: the
list is an ascending index, whose NULLs sort first, so its first entry is NULL if any is. A row
value's list (`(a, b) IN (SELECT ...)`) is still read through on a miss, as which of its rows are
equal but for NULLs depends on more than its first.

**Why.** On every miss the bytecode rewound the list and read all of it for a NULL, a step for each
entry. The suite's `subquery_in` (`grp = ? AND owner IN (SELECT id FROM owners WHERE region = ?)
LIMIT 50`) misses about nineteen rows in twenty against a list of about 200 owners; its profile
(fedora, 1 client) had 55% of its samples in the list's `Next`.

**Sources.** SQLite's `sqlite3ExprCodeIN` (`src/expr.c`): where its FALSE and NULL destinations are
one it codes the search as a single `OP_NotFound`; otherwise, for a scalar left side, it compares
the list's first row alone ("for a scalar LHS, it is sufficient to check just the first row of the
RHS"), and it reads a row value's list through. The answers themselves: SQLite's table of the IN and
NOT IN operators (`lang_expr.html`, "The IN and NOT IN operators"), ISO/IEC 9075-2's <in predicate>
as `= ANY` over the subquery's rows.

**Evidence.** `crates/engine/tests/in_subquery.rs`: 1,000 probes of a list of 2,000, 990 of them
misses, ran 5,961,023 instructions as a condition (bound 120,000) and 11,919,990 as values (bound
240,000); now both fall within the bound. Every form (a value, a condition, under `OR`, `NOT` and
`CASE`, a parameter or a column on the left, lists empty, without NULLs, with NULLs first, between
and last, row values with NULLs in either place) answers what a model of SQL's three values says,
before the change and after; mutants that drop the left side's NULL test, skip the list's NULL
check or read its last entry instead of its first each fail it. Release binaries on fedora (200,000
items, best of 3 in turns, each run started at load under 2, not recorded runs): `subquery_in` 213
→ 784 statements a second at 1 client and 2,846 → 6,833 at 32; `exists_subquery` and
`left_join_anti` unchanged.

**Falsified by.** An `IN` or `NOT IN` that answers other than SQL's three values; a list kept where
a NULL does not sort first (a descending or `NULLS LAST` index would make the first entry say
nothing).

## 3. LIKE jumps to where the literal after a `%` can match

**Decision.** Matching an ASCII pattern against ASCII text (`like_ascii`, the usual case), the
bytes after each `%` start where the pattern's next byte, a literal, next appears in the text in
either case (`like_literal_from`: `memchr2` on its lower and upper case), not at every position in
turn; a wildcard after the `%`, or the pattern's end, searches as before.

**Why.** The suite's `like_substring` (`body LIKE '%zephyr%' LIMIT 20` on 400 bytes of text, one
row in a thousand matching, so about 20,000 rows read) ran at 68 statements a second
(`bench/records/2026-10-21-fedora-suite-amphora-39549dc.jsonl`) against SQLite's 417 on the same
rows (`2026-10-21-fedora-suite.jsonl`): some 730 ns a row against 120. Every position of every text
was a try of the pattern's first literal, a compare and a branch a byte.

**Sources.** SQLite's `patternCompare` (`src/func.c`): past a `%` followed by an ASCII literal, it
finds the next position holding that character in either case with `strcspn` and tries the rest of
the pattern only there. `memchr2` (the `memchr` crate, already built for Amphora's parser) finds the
first of two bytes with vector compares, as glibc's `strcspn` does.

**Evidence.** `crates/engine/vdbe/value.rs`: `like_ascii` answers as `pattern_compare` (SQLite's
algorithm) for every pattern of up to four of `a`, `B`, `%`, `_` against every text of up to five
of `a`, `b`, `A`, `B`, and for thirteen patterns against texts of 0 to 1,000 bytes (the lengths
around 16, 32 and 64 that vector searches handle apart) holding the word early, late, in the other
case or not at all; with `memchr` searching the pattern's own case alone (a mutant), both fail. Release
binaries on fedora (200,000 items, best of 3 in turns, each run started at load under 2; checks,
not recorded runs): `like_substring` 67 → 222 statements a second at 1 client (p99 16.4 → 4.9 ms)
and 867 → 2,556 at 32 (p99 70.7 → 15.1 ms); `filter_no_index` and `point_text` unchanged. What is
left of SQLite's lead is mostly the table's pages read from the file, a few thousand a statement.

**Falsified by.** A text and pattern where `like_ascii` and `pattern_compare` disagree.

## 4. GROUP BY aggregates each row into its group in a hash table

**Decision.** A GROUP BY whose rows don't arrive in group order (no index gives it) aggregates each
row where it arrives, when its aggregates keep a few values each (COUNT, SUM, TOTAL, AVG, MIN, MAX;
no DISTINCT aggregate, no group_concat or JSON aggregate that grows with its group), no window runs
over its groups, its keys have no custom collation or type comparator, and no bare column sits
beside a MIN or MAX (whose row would choose its value). The row's keys are hashed as the hash join
and DISTINCT hash them (an integer and the equal real alike, text by its collation), its group found
or made, and the group's accumulators loaded into the aggregate registers for the row's steps and
saved back (`vdbe/group_table.rs`; `GroupFind`, `GroupSave`); a new group keeps the values its first
row gave the query's other non-aggregate expressions, as the sorter path keeps its first sorted
row's. After the scan the groups are sorted in the order the GROUP BY's sorter gives keys and
drained in it through the output subroutine a sorted group goes through, HAVING, ORDER BY and LIMIT
included (`GroupSort`, `GroupDrain`). The table holds at most the hash join's budget (64 MiB; none
with `temp_store=memory`): past it, a row of a group the table doesn't hold goes to the sorter as
every row did before, so a group is wholly in one or the other, and the sorter's groups are merged
with the table's by key as both are emitted, so the output keeps the sorter's order. Each run of the
GROUP BY begins with its table emptied (`GroupClear`): a correlated subquery runs it again within a
statement, perhaps after a LIMIT left groups undrained. `EXPLAIN QUERY PLAN` says `USE HASH TABLE
FOR GROUP BY`.

**Why.** Every grouped row went through the sorter: made into a record, sorted with every other row
(spilled to files past the sorter's memory), read back and compared with its neighbor, however few
the groups. The suite's `table_group_by` (`GROUP BY grp`: 20 groups over every row) completed no
statement within its two seconds at ten million rows (fedora, 2026-10-22, a check), and the GROUP BYs
over a twentieth of the table (`group_by_having`, `cte`: some 185,000 owners among 500,000 rows) ran
6 and 4 statements a second.

**Sources.** Graefe, "Query Evaluation Techniques for Large Databases" (ACM Computing Surveys 25(2),
1993), §4.2: aggregation by hashing reads its input once where the groups fit in memory, against
sorting's O(N log N), and overflow sends what doesn't fit to be partitioned or sorted. PostgreSQL's
HashAggregate (`src/backend/executor/nodeAgg.c`): a hash table of groups, each holding its
aggregates' transition states, spilling past `work_mem` since PostgreSQL 13. SQLite groups by sorter
or index alone (`src/select.c`, its GROUP BY coding), the path this replaces where it may.

**Evidence.** `crates/engine/tests/hash_group_by.rs` (the debug build's budget is 32 KiB, so the
larger tables overflow into the sorter): COUNT, SUM, MIN, MAX, TOTAL and a FILTERed COUNT over 2,000
rows in 40 groups and 6,000 rows in 5,000 groups, against an oracle, the groups in key order with
NULL first; ORDER BY the key descending, ORDER BY COUNT(*) DESC after a HAVING with a LIMIT, and a
bare LIMIT, past the budget; two-column keys, keys under NOCASE, and 1 and 1.0 as one group; a
correlated subquery whose GROUP BY runs once an outer row with a LIMIT leaving groups undrained;
the plan's hash table where the conditions hold and its sorter where they don't. Four mutants each
fail the tests written for them: no merge at the sorter's group boundaries, no table emptied at a
run's start, keys hashed as BINARY whatever their collation, and groups sorted ascending whatever
the order asked. `cargo test -p amphora-engine`: 2,766 tests pass, two planner tests now naming the
hash table where they named the sorter.

**Falsified by.** A GROUP BY whose hashed answer differs from its sorted one; a hashed GROUP BY
slower than the sorted one on the suite.

## 5. An ORDER BY with a LIMIT keeps only the rows its reader takes

**Decision.** An ORDER BY whose output a LIMIT cuts tells its sorter where the LIMIT (plus any
OFFSET) is: the register that holds it once evaluated, before the first row arrives (`SorterInsert`'s
`limit_reg`). At the first insert, a count from 1 to 65,536 (`MAX_KEPT`) makes the sorter keep only
that many rows, in a max-heap: a row enters while fewer are kept, or when its key orders before the
worst kept row's (compared from its decoded key, with no copy), which then leaves; the rest are
dropped as they arrive. Each kept row carries its arrival number, so rows that tie keep arrival order,
which is what the stable sort of every row gives them. The sort and the reads after it are the
sorter's own, OFFSET and LIMIT applied as before. A larger count, none, or a negative one (no limit)
sorts every row as before. DISTINCT deduplicates before the insert and a GROUP BY's groups arrive
through its output subroutine, so both keep their top rows the same way.

**Why.** Every ordered row went into the sorter, sorted with every other (spilled to files past the
sorter's memory), to emit a few. The engine had a heap for this (an ephemeral index the old row left
by `Last` and `Delete`), but it was chosen only where a LIMIT's registers already existed when the
ORDER BY's sorter was opened, which the main select path sets up only afterwards: an ordinary
`ORDER BY ... LIMIT` sorted every row. The suite's `order_by_sort` sorts some 500,000 rows at ten
million for its top 20; `group_by_having`, `cte` and `join_group` sort up to 185,000 groups for their
top 10, 5 and 10.

**Sources.** PostgreSQL's bounded heap sort (`src/backend/utils/sort/tuplesort.c`,
`tuplesort_set_bound`; EXPLAIN's "Sort Method: top-N heapsort"), used for a sort a LIMIT bounds, over
a scan's or an aggregate's output alike. SQLite's sorter limit (`src/select.c`, `pushOntoSorter`:
with a LIMIT, past LIMIT+OFFSET rows it deletes the last before each insert).

**Falsified by.** An ordered, limited answer that differs from the whole ordering's rows at its OFFSET;
a limited ORDER BY slower than the whole sort on the suite.

## 6. A subquery runs where it is read

**Decision.** A subquery outside the FROM clause is coded once, out of the line of code, as a
subroutine (`subquery.rs`, `emit_non_from_clause_subquery`): an uncorrelated one's body sits behind
its own `Once`, and a correlated one's runs for the row in hand. Each read calls it before using its
result (`ProgramBuilder::emit_lazy_subquery_call`): a scalar's or a row value's registers, an
EXISTS's, an IN's list, an IN that drives a seek, LIMIT and OFFSET. An uncorrelated subquery's call is
a `Once` and a `Gosub`, so it runs at most once, where control first reads it. A correlated one is
called at each read wherever what it reads of the outer row is there at the read: in WHERE, the select
list and ORDER BY of a query whose rows go straight out, for its loop's row; in an aggregate's or a
GROUP BY's output (the select list, HAVING, ORDER BY), from what the output keeps of the rows (a
group's carried columns; an ungrouped query's first row, as its bare columns); in a window query,
inside the window's source query, which takes the whole expression holding it (`window.rs`); in a
RIGHT or FULL JOIN's row body, which the unmatched rows' pass enters too; and in a write statement's
SET, WHERE and RETURNING, for the row in hand (`update.rs`, `delete.rs`, `insert.rs`), but for a row
value `SET (a, b) = (SELECT ...)`, read a register at a time, which runs once a row. Elsewhere (an
aggregate's arguments, a GROUP BY key, a RIGHT or FULL JOIN's WHERE) it still runs at every row of
its loop. A WHERE term holding a subquery is tested in the first loop,
after the terms that hold none, rather than before the loops (`planner.rs`,
`determine_where_to_eval_term`), but where a RIGHT or FULL JOIN's unmatched rows would skip that loop.

**Why.** Amphora ran each uncorrelated subquery before its loops, read or not, and each correlated one
at every row of its loop. One that no row reaches still ran and raised its error where SQLite answers:
the oracles' finding error-99fb1658e34f78cb (sum's integer overflow under `WHERE 1 OR (SELECT
sum(x) ...)`), and with it the same subquery over an empty table, past a WHERE term no row passes,
in a CASE branch no row takes, or beside `WHERE 0`; and a correlated one in a CASE branch of an
aggregate's output, a GROUP BY's, a window query's, a RIGHT JOIN query's or an UPDATE's SET ran at
every row (`SELECT count(*), CASE WHEN max(a.c0) = 99 THEN (SELECT ... a.c0 ...) END`: SQLite
3|NULL, Amphora integer overflow). Its work was spent for nothing too.

**Sources.** SQLite 3.54.0's `sqlite3CodeSubselect` (`src/expr.c`) codes a subquery where it is first
read, as a subroutine (`OP_BeginSubrtn` ... `OP_Return`) whose body sits behind `OP_Once` unless it
is correlated; a later read is an `OP_Gosub` to it. `sqlite3WhereBegin` (`src/where.c`) tests a WHERE
term before its loops (its False-WHERE-Term-Bypass) only if it reads none of the loops' tables and
`exprIsDeterministic` holds, which fails on any subquery (`sqlite3SelectWalkFail`);
`sqlite3WhereCodeOneLoopStart` (`src/wherecode.c`) codes a term holding a correlated subquery
(`TERM_VARSELECT`) after the loop's other terms. In an aggregate query a reference to the query's own
columns is an aggregate column (`TK_AGG_COLUMN`, `analyzeAggregate`), read from the values kept for
the output, so the output's subqueries run there; a window query's select list is evaluated in the
parent of the subquery `sqlite3WindowRewrite` makes; a RIGHT JOIN's unmatched rows run the row body
in a loop of their own (`sqlite3WhereRightJoinLoop`).

**Evidence.** `crates/engine/tests/oracle_findings.rs`: `an_unread_subquery_never_runs` and
`an_unread_correlated_subquery_never_runs` failed before the change with the subquery's integer
overflow; with it they answer as SQLite 3.54.0 does, both where nothing reads the subquery and where
something does (a seek key, an IN that drives a seek, LIMIT, an aggregate's output, which raise the
error as SQLite does). `a_subquery_read_in_two_places_is_coded_once` (GROUP BY and the select list,
an UPDATE's row value) finds the subquery listed once by EXPLAIN QUERY PLAN and its body coded once,
and `a_triggers_subqueries_run_afresh_at_each_firing` finds a trigger's subqueries run afresh at each
firing. `a_correlated_subquery_no_row_reads_never_runs` failed before the correlated subqueries
moved, in an ungrouped aggregate's output, and answers as SQLite with it in every place: the
aggregate, a GROUP BY's select list and HAVING, a window query (one a LIMIT cuts short too), a RIGHT
JOIN query, a SELECT whose rows go straight out, an UPDATE's SET and a DELETE's WHERE, unread and
read. Release binaries before and after, best of 3 in turns at load under 2 on adas-mac-studio
(Apple M4 Max, 14 cores; `bench/records/2026-10-25-adas-mac-studio-lazy-subqueries.md`): time per
statement within 1% for the suite's `subquery_in` and `exists_subquery` at 1 and 32 clients, and
their median alone unchanged; `exists_subquery`'s median at 32 clients, 1.061 in the best of 3, lay
within its own run-to-run spread in five more rounds (the median run 0.988 after over before). With
the correlated subqueries moved, on adas-mac-studio-2
(`bench/records/2026-10-25-adas-mac-studio-2-corr-subqueries.md`): the suite's `subquery_in`,
`exists_subquery` and `group_correlated` within 1% in time per statement, the same programs before
and after (the last is unnested at the suite's size), and `group_correlated_row`, a correlated
subquery in a grouped query's output, 15.8 times as fast at 1 client and 11.4 at 32, its subquery run
once a group instead of once an input row.

**Falsified by.** A subquery that runs where SQLite's doesn't (an error SQLite doesn't raise); an
uncorrelated one run more than once in one run of the query that holds it, or a correlated one run
twice for one read; a subquery statement of the suite slower than before.
