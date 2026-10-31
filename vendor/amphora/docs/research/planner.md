# The planner: techniques at their sources

Source notes for Amphora's planner (CLAUDE.md §5), from kiln research
`01a16eab7f47731291bbc7e2140f5e8a` (plan "The planner, researched and audited", the person's ask of
2026-10-24). Six topics: cardinality estimation, join ordering, unnesting, cost models, robust and
adaptive execution, and plan caching. For each, this note says what the papers found, what DuckDB,
Umbra/CedarDB, PostgreSQL, SQL Server, CockroachDB, ClickHouse and Calcite do, and where Amphora
stands. The planner's decisions are in `docs/design/planner.md`. This note is what they cite.
Section 7 audits Amphora's planner against these sources, each gap shown on a release build;
section 8 is the baseline of its estimates against the rows counted on JOB and TPC-H; section 9
ranks the changes they call for, each with the measurement that would show it.

Every claim ends with its evidence in parentheses: the source, where in it, the words quoted, then
the research finding (`f11` to `f43`) and the grade kiln gave it (A to F, from the sources' kinds,
how far the quotes held up where cited, how many independent sources agree, and what is against
it). A grade is kiln's judgment of the evidence, not of the technique. Kiln doesn't read a PDF's
words, so a finding resting on one paper's PDF alone (f38, f40) is graded C with its quotes
unchecked. Those quotes were taken from the papers' text, kept under `~/.cache/planner-research/`.
Where a finding has a
counterclaim, the counterclaim is given beside it. The engines' code was read at fixed revisions:
DuckDB main at 1641475e (2026-10-23), PostgreSQL master at 126976a8 (2026-10-23), CockroachDB
master at d30c905f (2026-10-24), and ClickHouse master at eddf9eec (2026-10-23). Amphora's own
findings (f1 to f10, f36, f37) cite its tree as of 2026-10-24.

## Where Amphora's planner stands

- Statistics are SQLite's `sqlite_stat1` only: a table's row count, and per index the average
  rows per distinct value of each leftmost prefix. Without `ANALYZE` it assumes a million rows a
  table and fixed selectivities (crates/engine/stats.rs line 18: "Average number of rows per
  distinct key prefix, for each leftmost prefix"; cost_params.rs lines 106-118:
  "rows_per_table_fallback: 1_000_000.0,"; f2, A).
- A table's filters multiply (the independence assumption). The one exception is a lower and an
  upper bound on one column, which count as a closed range of 0.005 (cost.rs line 429:
  "fraction *= constraint.selectivity;"; f3, A).
- Join estimates were made independent of join order in the working tree this week (join.rs line
  130: "a join's estimate does not depend on the order its tables are read"; f4, A). CERT is not
  yet clean: campaign 2 ran 21,848 checks, 37 failed and 2,049 were skipped for want of an
  estimate; 35 were cause AF (fixed) and 2 remain open as cause AG (bench/oracles/findings.md line
  67: "AG. CERT, open: a FULL JOIN's estimate below its inner join's; an index seek's estimate
  against two filters'"; f5, B). Without statistics, an estimate can jump by up to five orders of
  magnitude when a query is made more restrictive: a cross join with one added conjunct went from
  1,000,000 to 160,000,000,000 rows (cert-50d4acc3fb3a85cd.md line 1: "the more restrictive query
  is estimated at 160000000000.00003 rows, the query at 1000000"; f6, B). Since then step 5 has
  closed AG with the other causes its campaigns found, and CERT failed none of its 2,607,858 checks
  on 10f2461 (bench/records/2026-10-27-adas-mac-studio-2-cert-clean/README.md).
- Join order: left-deep only, by a dynamic program over table subsets each extended by one table up
  to 12 tables, then a greedy left-deep search (join.rs line 1576: "In this block, LHS is always
  (n-1) tables, and RHS is a single table."; line 1813: "pub const GREEDY_JOIN_THRESHOLD: usize =
  12;"; f1, A). A RIGHT or FULL JOIN is a reordering barrier in both directions (join.rs line 2000;
  f9, A).
- Unnesting is by fixed patterns: EXISTS and NOT EXISTS become semi- and anti-joins, a positive IN a
  semi-join, and a one-aggregate subquery is grouped by its correlation columns. NOT IN stays a
  subquery, and nothing is unnested beside a RIGHT or FULL JOIN (unnest.rs line 106: "`NOT IN` stays
  as a subquery because NULL values can change its result."; f7, A).
- Cost is one abstract unit over page reads, seeks, row, filter, hash and sort work, with Mackert
  and Lohman's finite-LRU formula for table lookups (OPTIMIZER.md line 62; f9, A).
- A statement is planned at prepare and again only when re-prepared after a schema change. There is
  no shared plan cache, and a parameter's value never causes a new plan (f8, B).
- Per-operator estimation error can't be measured yet: metrics are whole-statement totals and
  `EXPLAIN QUERY PLAN` carries estimates but no actual counts (vdbe/metrics.rs line 72: "pub
  rows_read: u64,"; f36, B). The workloads to judge the planner are at hand: JOB's 113 queries are
  cloned locally, and the G2 corpus goal names TPC-H, TPC-DS, JOB, ClickBench, SSB and CH-benCHmark
  (f37, A).
- The rules bound how any change is built: no recursion, no depth-first walk of user-scaled
  structures on the call stack, and every loop over user-scaled data ends on a counted budget
  (CLAUDE.md line 86: "No function calls itself, directly or through others, and no algorithm walks
  a user-scaled structure"; f10, A). This matters below: DPhyp as published, DuckDB's enumerator and
  both Neumann unnesting papers are stated recursively.

## 1. Cardinality estimation

### What the sources found

**Estimates are the main cause of bad plans.** On the Join Order Benchmark (113 multi-join queries
over IMDb), every estimator tested routinely erred by a factor of 1,000 or more. The errors grew
exponentially with the number of joins, and multi-join results were systematically underestimated,
which the authors trace to the independence assumption PostgreSQL makes (Leis et al., PVLDB 9(3)
2015, line 82: "113 multi-join queries"; section 3.1 line 575: "we routinely observe misestimates
by a factor of 1000 or"; line 577: "the errors grow exponentially (note the logarithmic scale)";
line 625: "tend to systematically underestimate"; section 2.3 line 306: "PostgreSQL simply assumes
independence and multiplies the selectivities"; LpBound's abstract: "is often the main culprit when
the optimizer chooses a poor plan"; f11, A).

**Sampling.** Systems that keep table samples estimated single-table results considerably better on
JOB. The authors urge "table samples as a simple and robust technique", but single-table samples
miss join-crossing correlations (Leis et al. 2015, section 7 line 1500: "systems should be adopting
table samples as a simple and robust technique"; line 1537: "many of our JOB queries contain
join-crossing correlations, which single-table samples do not capture"). Index-based join sampling,
run under a budget of index lookups, gave "much better plans than traditional estimators" (Leis,
Radke et al., CIDR 2017, section 3 line 382: "during sampling and stops once the given budget (e.g.,
100,000 index lookups) is exhausted."). Oracle samples table blocks at parse time when statistics
are missing, stale or blind to correlation (Oracle 19c SQL Tuning Guide, Supplemental Dynamic
Statistics: "The database uses recursive SQL to scan a small random sample of table blocks."; f13,
D). Against it: on STATS-CEB, Han et al. found uniform and wander-join sampling over joins worse end
to end than PostgreSQL's own estimator. Neither is index-based join sampling or a per-table sample
used for filters (Han et al., PVLDB 15(4) 2022, section 5.1, O1: "Traditional histogram and
sampling based methods"; strong). Kiln grades f13 D for that counterclaim and for quotes it could
not check.

Umbra keeps its samples fresh online. Inserts maintain the sample as they happen, with "virtually
zero overhead in insert performance" and none of the random I/O a rebuilt sample costs; the price
is relaxed freshness across threads. A deleted sampled tuple is replaced by an upcoming one at the
same position (Birler, Radke, Neumann, DaMoN 2020, abstract: "It is much more attractive to use
online sampling, such that a fresh sample is available at all times, without additional data
accesses."; abstract: "We can trade off strict freshness guarantees for a significant boost in
performance"; section Updates and Deletes: "our online approach needs to pick an upcoming tuple to
replace the deleted tuple."; Umbra, CIDR 2020, section 3 line 568: "a scalable online reservoir
sampling algorithm that we recently developed [2]"; f41, A).

**Sketches.** HyperLogLog estimates cardinalities beyond 10^9 "with a typical accuracy of 2% while
using a memory of only 1.5 kilobytes." (Flajolet et al., AofA 2007, abstract line 22). Umbra keeps
updatable HyperLogLog sketches on every column beside its sample. Together they give almost perfect
per-column distinct counts and, with the sample, accurate multi-column estimates (Neumann and
Freitag, CIDR 2020, section 3 line 525: "Umbra tracks a number of statistics for query optimization
purposes. First of all, a random sample of each relation is maintained."; line 572: "shown
previously [6], our implementation of updateable HyperLogLog sketches can provide almost perfect
cardinality estimates"; Freitag and Neumann, CIDR 2019, abstract: "with random sampling to correct
for correlation bias between"; f14, A).

**Correlated columns.** PostgreSQL's docs say "It is common to see slow queries running bad
execution plans because multiple columns used in the query clauses are correlated." Their extended
statistics must be created by hand because "it's impractical to compute multivariate statistics
automatically." (PostgreSQL docs 14.2.2; clausesel.c line 70: "probabilities, and in reality they
are often NOT independent even if they"; f17, A). SQL Server's CE 120 and later assumes partial
correlation, combining selectivities by "a variation on exponential backoff, ordering the
selectivities from most to the least selective predicate." CE feedback adjusts that per query and
keeps "Only verified feedback" (Microsoft Learn, CE feedback; f17, A).

**Learned estimators.** On static data they are more accurate than traditional ones, but they cost
much to train and run, can't keep up with fast updates, and are "much harder to interpret and
often unpredictable." A simple adaptive-execution approach matched or beat reinforcement-learned
optimizers with no training (Wang et al., PVLDB 14(9) 2021, abstract: "learned models are indeed
more accurate than traditional methods"; Zhang et al., PVLDB 16(11) 2023, abstract line 41:
"competitive to the SOTA RL-based approaches but, in some cases,"; f18, D). Against it: Han et
al.'s end-to-end benchmark found ML data-driven estimators (BayesCard, DeepDB, FLAT) markedly
improving PostgreSQL's query times, where most traditional methods "hardly improve the PostgreSQL
baseline." (section 1, K1; strong). Kiln grades f18 D for that.

**Pessimistic bounds.** LpBound computes "a guaranteed upper bound on" multi-join outputs from
lp-norms of degree sequences. It was orders of magnitude more accurate than mainstream estimators
on JOB and STATS, and with its bounds PostgreSQL chose plans "at least as good as those derived
using" true cardinalities (Zhang et al., arXiv:2502.05912, abstract). In Han et al.'s independent
benchmark, the pessimistic estimator PessEst also did significantly better than PostgreSQL (section
5.1, O1: "whereas the new traditional approach"; f19, A). SafeBound made bounds practical with
predicates and compressed degree sequences: "By never underestimating, this approach allows the
optimizer to avoid potentially inefficient plans." It "achieves up to 80% lower end-to-end runtimes
than PostgreSQL" and "saves up to 500x in query planning time". Its authors say "the current
SafeBound prototype is likely not yet ready for a production system": "Any upper bound system must,
at some point, read all rows in the database."; "A challenge that we leave open is handling updates
without recomputing the degree sequences."; and several predicates on one table give "an estimated
cardinality equal to the selectivity of the most selective predicate." (Deeds, Suciu, Balazinska,
SIGMOD 2023, abstract and section Limitations and Future Work; f40, C).

**Measuring error: q-error and P-error.** The q-error is the factor between an estimate and the
truth, and it bounds a plan's cost at "a factor of q 4" times the optimum (Moerkotte, Neumann,
Steidl, PVLDB 2(1) 2009, abstract). But it "cannot distinguish the importance of" sub-plans and
doesn't track end-to-end time. Han et al.'s P-error "is highly correlated with the end-to-end
query time improvement." (Han et al. 2022, abstract and section 1; f20, A).

**CERT's evidence.** CERT (cardinality restriction monotonicity) checks that a more restrictive
query, such as a LEFT JOIN made INNER or a conjunct added, is never estimated to return more rows:
its estimate "should not exceed the estimated number of rows for the original". It targets
estimation because estimation "was shown to be the most important part for query optimization". It
found 13 unique issues in MySQL, TiDB and CockroachDB (Ba and Rigger, ICSE 2024, abstract: "We
evaluated CERT on three widely used and mature DBMSs, MySQL, TiDB, and CockroachDB."; "CERT found 13
unique issues, of which 2 issues were fixed and"; f21, A). Amphora's oracles run it (f5, f6 above).

### What the engines do

| engine | estimation | evidence |
|---|---|---|
| DuckDB | A join's cardinality is a property of its relation set, cached per set: base cardinalities over a denominator built from the join predicates' equivalence-class domains. Distinct counts come from HyperLogLog over a 10% sample of each vector (30% for integers). | cardinality_estimator.cpp line 188: "reference_map_t<JoinRelationSet, CardinalityHelper> relation_set_2_cardinality;"; line 902: "The estimator starts with base relation cardinalities and divides by a denominator assembled from predicate domain"; distinct_statistics.hpp lines 52-54: "static constexpr double BASE_SAMPLE_RATE = 0.1;" (f15, A) |
| DuckDB (its source) | Ebergen's thesis assumes every join is a foreign-key/primary-key join (containment of value sets). Transitively joined columns form equivalence sets sharing one total domain: the largest known distinct count, or else the lowest-cardinality column taken as the primary key. Each dynamic-programming entry's cardinality is a property of its relation set. Without statistics, every filter gets a flat 20%. On JOB: 66% fewer intermediate tuples on Parquet, 95% on base tables, and 18% and 25% faster end to end. | thesis abstract: "By assuming all joins are foreign-key primary-key joins"; "join order optimizer improved end-to-end execution times by 18% on average"; section 3.1: "Every entry in the dynamic programming table is represented as a set of relations, and now the cardinality is also represented as a set of relations."; section 3.2: "then the column with the lowest cardinality in the equivalence set is assumed to be a primary key"; section 3.4: "selectivity of 20%. In the presence of HyperLogLog statistics"; DuckDB cites it at cardinality_estimator.cpp line 900 (f39, A) |
| Umbra / CedarDB | A random sample of every relation, kept by online reservoir sampling, plus updatable HyperLogLog on every column. CedarDB says its statistics estimate group-by result sizes. | Umbra CIDR 2020 section 3 lines 525, 568, 572 (above); CedarDB docs, State-of-the-art query optimizer: "estimate relation size after group by operations" (f14, A; f41, A) |
| PostgreSQL | One rowcount for every path of a join relation. Extended statistics (functional dependencies, multi-column distinct counts, MCV lists) are applied first, by hand; the remaining clauses multiply. | costsize.c line 5679: "anyway we must keep the rowcount estimate the same for all paths for the"; clausesel.c line 68: "The remaining clauses are then estimated by taking the product of their" (f16, A; f17, A) |
| SQL Server | Partial correlation by exponential backoff (CE 120 and later), CE feedback per query, and a containment assumption for joins. CE 70's simple containment applies each input's filter selectivity to its histogram before joining histograms. CE 120's base containment estimates the join from the base tables' histograms, then applies the filters, and assumes filters on different tables are uncorrelated. A hint restores simple containment. | Microsoft Learn, Cardinality Estimation, Versions of the CE: "Users query for data that exists. For example, for an equality join between two tables, factor in the predicates selectivity"; "Users might query for data that doesn't exist. For example, for an equality join between two tables, we use the base tables histograms to estimate the join selectivity, and then factor in the predicates selectivity."; Example C: "predicate filters from different tables usually don't correlate with each other."; Query Hints, ASSUME_JOIN_PREDICATE_DEPENDS_ON_FILTERS: "Generates a query plan using the Simple Containment assumption instead of the default Base Containment assumption for joins"; Versions of the CE: "The combination of the different column values isn't necessarily independent." (f43, A; f17, A) |
| CockroachDB | Every expression in a memo group shares the group's logical properties, row count among them. CERT found issues in its estimator. | memo.go line 62: "expressions in a group share the same logical properties" (f16, A); CERT abstract (f21, A) |
| ClickHouse | Column statistics, GA since 26.3 and used by default, built automatically on every suitable column (basic and uniq_v2 by default since 26.7) and materialized on insert. t-digest answers less-than predicates, count-min sketches equalities (0.1% error at 99.9% confidence), uniq (HyperLogLog) distinct counts. Columns multiply (independence). Without statistics, fixed factors: 0.33 a range, 0.01 an equality. | Settings.cpp line 10803: "Column statistics are now GA."; line 2416: "so cost-based join reordering has good estimates on freshly-loaded dimension tables."; MergeTreeSettings.cpp lines 2465-2467: "Supported statistics types: basic, tdigest, countmin, uniq, uniq_v2."; Statistics.cpp line 340: "TDigest: '< X' (less-than predicates)"; StatisticsCountMinSketch.cpp lines 21-22: "an error tolerance of 0.1% (ε = 0.001)"; ConditionSelectivityEstimator.cpp line 1016: "The map enforces independence across columns"; ConditionSelectivityEstimator.h line 164: "static constexpr Float64 default_cond_range_factor = 0.33;" (f42, A) |
| Calcite | Estimation is not in the research; its cost combines CPU, I/O and memory (section 4). | (f29, A) |

The three planners that make a join's estimate a property of the set of relations joined, not of
the path or order that builds it, are PostgreSQL, CockroachDB and DuckDB. That makes an estimate
order-independent by construction (f16, A). Amphora's AF fix gets the same property differently:
a constraint is ready once all its tables are joined (f4).

## 2. Join ordering

### What the sources found

**Exact enumeration: DPsize, DPccp, DPhyp.** DPsize, which Amphora's subset-by-size search
resembles, runs far above the lower bound. DPccp meets it, and DPhyp extends DPccp to hyperedge
predicates and non-inner joins, improving optimization time "by orders of magnitude, compared to
known algorithms dealing with complex join predicates and non-inner joins." As published, DPhyp's
subgraphs and complements "are created by recursive graph traversals" (Moerkotte and Neumann,
SIGMOD 2008, section 1 line 26: "We proved that DPsize has a runtime complexity"; line 65: "DPhyp,
which is capable to handle complex join predicates"; section 3 line 295; f22, A).

**Conflict detection for outer, semi and anti joins.** ClickHouse and CockroachDB reorder non-inner
joins exactly with the CD-C conflict detector of Moerkotte, Fender and Eich, which is "correct and
complete". ClickHouse runs it in DPsub; CockroachDB runs it in DPSube (ClickHouse Settings.cpp line
11355: "the CD-C conflict detector, which is correct and complete"; CockroachDB
join_order_builder.go line 137: "The paper 'On the correct and complete enumeration of the core
search space'"; f25, A). Amphora treats RIGHT and FULL JOIN as barriers instead (f9).

**Heuristics leave performance on the table.** On JOB, exhaustive dynamic programming beat
heuristics even with estimation errors: with true cardinalities and PK+FK indexes, GOO's plans were
up to 21x costlier and Quickpick's up to 32x. Left-deep trees cost at most 4.5x the best bushy plan,
and zig-zag trees at most 2.54x (Leis et al. 2015, section 1 line 142: "using heuristics leaves
performance on the table"; section 6.2 line 1491: "being 2.54× more expensive than the best bushy
plan"; f27, A). Greedy Operator Ordering as published keeps a set of trees and merges the pair with
the lowest cost, so it builds bushy trees. The Amphora search labelled GOO instead adds one table at
a time to a left-deep tree (Leis et al. 2015 lines 1452-1453: "The algorithm then combines the pair
of join trees with the lowest"; Neumann and Radke 2018 line 273: "the best known greedy heuristics
is Greedy Operator Ordering [8],"; Amphora join.rs line 1817: "Builds a left-deep join tree by:";
f28, A).

**Adaptive and greedy hybrids for large queries.** Neumann and Radke adapt to the query graph
rather than switching to greedy at a fixed table count. They run DPhyp outright below 14 relations.
Up to 100 relations, they "count the number of connected subgraphs up to the budget of 10,000" and
solve exactly within it. Beyond that they use search-space linearization, near-optimal up to 100
relations, and add a greedy step only for larger queries. Real queries reach "more than 4,000
relations" (SIGMOD 2018, lines 377-380, 325: "But instead of switching to a greedy approach, as some
other systems do, it uses a novel search space linearization technique to"; line 45; f23, A).

### What the engines do

| engine | join ordering | evidence |
|---|---|---|
| DuckDB | DPhyp; exact search stops after 10,000 emitted pairs, then Greedy Operator Ordering; approximate from 12 relations (`approximate_join_order_threshold`). The connected-subgraph enumeration is a recursive function. | plan_enumerator.cpp line 580: "the plan enumeration is a straight implementation of the paper "Dynamic Programming Strikes Back" by Guido"; lines 243-244: "when the amount of pairs gets too large we exit the dynamic programming and resort to a greedy algorithm"; line 410; line 348: "bool PlanEnumerator::EnumerateCSGRecursive(JoinRelationSet &node, unordered_set<RelationIndex> &exclusion_set) {"; settings.hpp lines 312-316: "The minimum number of tables in a join to determine the optimal join order approximately instead of exactly." (f24, A; f22, A) |
| Umbra / CedarDB | Neumann and Radke's adaptive scheme (above). CedarDB says its optimizer is "extensively tested for extremely large queries" (more than 10,000 joins). | CedarDB docs, State-of-the-art query optimizer: "full-fledged cost-based query optimizer extensively tested for extremely large queries" (f23, A) |
| PostgreSQL | Bushy trees without cross products, searched exhaustively up to the collapse limits (8 FROM items by default); GEQO from 12 FROM items. | PostgreSQL docs 19.7, geqo_threshold: "Use genetic query optimization to plan queries with at least this many"; "The default is 12."; from_collapse_limit: "The default is eight."; Leis et al. 2015 line 298: "Join orders, including bushy trees but excluding trees with" (f26, A) |
| SQL Server | Not in the research. Its join decisions under uncertainty are in section 5. | — |
| CockroachDB | DPSube with CD-C; reorders at most 8 joins by default. | join_order_builder.go line 123: "Then, the DPSube algorithm is executed (see citations: [8]). DPSube"; docs, Join reordering: "only used to reorder subtrees containing 8 or fewer joins by default" (f25, A) |
| ClickHouse | Default `dpsub,greedy` since 26.11: DPsub with CD-C up to 12 relations, then greedy, under a deterministic budget of 100,000 partial plans. DPhyp offered for inner joins only. | Settings.cpp line 11340: "DECLARE(JoinOrderAlgorithm, query_plan_optimize_join_order_algorithm, "dpsub,greedy", R"("; line 11344: "It only applies to queries with at most 12 relations; above that it gives way to the next algorithm in the list."; line 11345: "implements DPhyp (Dynamic Programming via Hypergraph Partitioning) algorithm currently only for inner joins"; line 3130: "Maximum number of partial plans the join order optimizer may enumerate before giving up and falling back to the next algorithm"; line 11342: "basic greedy algorithm - works fast but might not produce the best join order" (f25, A; f22, A; f27, A) |
| Calcite | A Volcano-style cost-based dynamic program over rule-generated alternatives, and a heuristic Hep planner firing rules to a fixed point. Large joins are ordered by the heuristic LoptOptimizeJoinRule. | Begoli et al., SIGMOD 2018, section 6: "uses a dynamic programming algorithm, similar to Volcano"; "which triggers rules exhaustively until it generates an expression that is no longer modified by any rules"; LoptOptimizeJoinRule javadoc: "implements the heuristic planner for determining optimal join orderings" (f29, A) |

## 3. Unnesting

### What the sources found

**Dependent joins (2015).** Correlated subqueries "frequently lead to dependent joins with nested
loops evaluations and thus poor performance". Neumann and Kemper turn every correlated subquery
into a dependent join and push it down until no correlation is left, for arbitrary queries. Earlier
systems used heuristics limited to classes of queries (BTW 2015, abstract line 9; line 13: "no
existing system can de-correlate queries in the general case. We present a generic"; f30, B).

**Top-down unnesting (2025).** When correlated subqueries nest, the 2015 method re-unnests
fragments. Neumann's single-pass top-down variant fixes this, visiting each operator at most once
(arXiv:2412.04294 abstract: "Recent work has introduced a single pass top-down algorithm for
unnesting arbitrary SQL queries."; section 5 line 952: "The unnesting algorithm visits each operator
at most once, starting from dependent joins in top-to-bottom order and stopping when the current
tree no longer"; f30, B). The BTW 2025 paper behind it says existing techniques "are either limited
in scope or lead to suboptimal execution plans when correlated queries are stacked repeatedly inside
each other". The 2015 algorithm is used by HyPer, DuckDB and Umbra, and "The original algorithm
does that bottom-up." The new one "can be seen as a top-down variant of [NK15]" and adds "recursive
SQL and ORDER BY LIMIT constructs." On the paper's Figure 1 query, PostgreSQL 16 needs over 26
minutes on TPC-H SF1; "After applying our algorithm, Postgres can execute the query in 1.3s, a
speed-up of more than 1000x." Umbra with the old logic exhausts 120 GB, while "Using the new
top-down formulation it finishes in 33ms on the same machine." Its per-operator rules are stated as
recursion ("and recurse to the input"), so Amphora would need them as a loop over an explicit
worklist (f10) (Neumann, BTW 2025, abstract p. 25, section 1 pp. 26-27, section 3 p. 31, section
3.3 p. 36, section 5 p. 44; f38, C).

### What the engines do

| engine | unnesting | evidence |
|---|---|---|
| DuckDB | Decorrelates every subquery with the 2015 algorithm. | DuckDB blog 2023-05-26: "DuckDB uses a state-of-the-art subquery decorrelation algorithm as described in the" (f31, A; f30, B); BTW 2025 p. 26: "Both Hyper and DuckDB use it in commercial offerings, our research system Umbra uses it" (f38, C) |
| Umbra / CedarDB | Umbra has integrated the 2025 top-down method. CedarDB fully decorrelates complex dependent subqueries, quadratic to linear. | BTW 2025 section 7: "The new approach has already been integrated into our Umbra system and we hope that other systems like DuckDB and Postgres will follow." (f38, C); CedarDB docs: "fully decorrelate very complex dependent subqueries"; "reducing them from quadratic runtime to linear runtime" (f31, A) |
| PostgreSQL | Doesn't unnest the Figure 1 query: over 26 minutes on TPC-H SF1, 1.3 s once the algorithm is applied by hand. | BTW 2025 section 5, p. 44 (f38, C) |
| SQL Server | Not in the research. | — |
| CockroachDB | Not in the research. | — |
| ClickHouse | Not in the research. | — |
| Calcite | Not in the research; its rules are applied by the two planner engines (section 2). | (f29, A) |

Amphora unnests by fixed patterns and cites both Neumann papers as references (f7).

## 4. Cost models

**Estimates matter more than costs.** On JOB, cost-model errors were "dwarfed by cardinality
estimation errors" and simple cost models sufficed. DuckDB's join-order cost is essentially each
join's estimated cardinality plus its inputs' costs (Leis et al. 2015, section 1 line 138: "cost
model errors is dwarfed by cardinality estimation errors"; abstract line 44: "we find that it has
much less influence on query performance than"; DuckDB cost_model.cpp line 37: "Currently cost of
a join mostly factors in the cardinalities."; line 47: "return join_cost + left.cost +
right.cost;"; f12, D). Against it: Amphora's own bench found plans doing far more work from
cost-model gaps, not estimates: a sort charged to the wrong candidate, and table lookups charged too
few pages. Those were fixed in planner.md decisions 2, 5 and 7 (docs/design/planner.md, decision 5:
"Mackert and Lohman count about 188,000 pages read for 200,000"; strong). Kiln grades f12 D for
that. For Amphora's point and range statements, the cost model mattered.

| engine | cost model | evidence |
|---|---|---|
| DuckDB | Join cost is the cardinality of each join plus its inputs' costs. | cost_model.cpp lines 37, 47 (f12, D) |
| Umbra / CedarDB | Not in the research. | — |
| PostgreSQL | Not in the research beyond JOB's finding that its cost model mattered less than its estimates. | Leis et al. 2015 (f12, D) |
| SQL Server | Not in the research. | — |
| CockroachDB | Not in the research. | — |
| ClickHouse | Not in the research. Its join order runs under a budget of partial plans (section 2). | (f25, A) |
| Calcite | Default cost combines CPU, I/O and memory estimates. | Begoli et al. 2018, section 6: "CPU, IO, and memory resources used by a given expression." (f29, A) |

## 5. Robust and adaptive execution

**Guards on decisions that hinge on estimates.** SQL Server's batch-mode adaptive join defers the
choice between hash join and nested loops until its first input is scanned: it "defines a threshold
that is used to decide when to switch to a Nested Loops plan." and "Your plan can therefore
dynamically switch to a better join strategy during execution." (Microsoft Learn, Intelligent query
processing details). On JOB, nested-loop joins without an index were the risky choice: "this
decision is extremely risky". With them removed, "we observed no more timeouts" under PostgreSQL's
estimates (Leis et al. 2015, section 4.1 lines 894, 899; f32, A).

**Execution-time robustness.** Lookahead Information Passing pushes Bloom filters through left-deep
star-join pipelines (Zhu et al., PVLDB 10(8) 2017, section 1 line 25: "techniques that increase the
robustness of query plans, by"). Predicate transfer pre-filters every table through the join graph
with Bloom filters: 3.3x over Bloom join on TPC-H, and "regardless of the chosen join order, the
join phase" is robust (Yang et al., CIDR 2024, section 2 line 134). POLAR routes tuples among
alternative left-deep orders in DuckDB with bounded regret, "of up to 9x and less than 7% overhead
for all benchmark queries," (Justen et al., PVLDB 17(6) 2024, abstract line 70). Diamond-hardened
joins split joins into Lookup and Expand: up to 500x on the CE benchmark, with no loss on TPC-H or
JOB (Birler, Kemper, Neumann, PVLDB 17(11) 2024, abstract line 43: "to traditional joins, queries’
performance is improved by up to 500x"; f33, A).

**Re-optimization.** A simple mid-query re-optimization on observed cardinalities cut the
end-to-end latency of JOB's 20 longest-running queries by 27%, most of what perfect estimates would
give: re-optimization "is likely the most cost-effective way" (Perron et al., ICDE 2019, abstract:
"queries in the Join Order Benchmark by 27%, realizing most of"). Holistic adaptive processing is
still absent from commercial systems because it complicates debugging and testing (POLAR, abstract
line 60: "challenges. However, commercial database systems in practice do"; f34, A).

| engine | robust or adaptive execution | evidence |
|---|---|---|
| DuckDB | POLAR was built and measured in DuckDB (a research prototype, not DuckDB's release). | (f33, A) |
| Umbra / CedarDB | Diamond-hardened joins come from Umbra's group. The research does not say whether CedarDB ships them. | (f33, A) |
| PostgreSQL | No adaptive join in the research. JOB's timeouts under its estimates ended once unindexed nested loops were removed. | (f32, A) |
| SQL Server | Batch-mode adaptive joins: hash or nested loops chosen at run time against a threshold, within one cached plan. | (f32, A) |
| CockroachDB | Not in the research. | — |
| ClickHouse | Not in the research. | — |
| Calcite | Not in the research. | — |

## 6. Plan caching

Parameter-sensitive plans are handled three ways (f35, A):

- **Custom then generic.** PostgreSQL and CockroachDB plan a prepared statement with its values for
  the first five executions, then switch to a generic plan if its estimated cost is not
  significantly above those custom plans' average. PostgreSQL's PREPARE docs: "first five
  executions are done with custom plans and the average estimated cost of those plans is
  calculated."; 19.7, plan_cache_mode: "Custom plans are made afresh for each execution using its
  specific set of parameter values, while generic plans do not rely on the parameter values and
  can be re-used across executions."; CockroachDB docs, Query plan type: "Subsequent executions use
  a generic plan if its estimated cost is not significantly higher than the average cost of the
  preceding custom plans."
- **Several cached variants.** SQL Server's Parameter Sensitive Plan optimization keeps several
  plans for one parameterized query, dispatched by the parameters' estimated cardinality ranges:
  "It addresses the scenario where a single cached plan for a parameterized query isn't optimal for
  all possible incoming parameter values." (Microsoft Learn, PSP optimization, introduction).
- **Recompile on a new value.** SQLite recompiles a prepared statement when a newly bound value
  could change its plan: it "will be automatically recompiled, as if there had been a schema
  change" (sqlite3_prepare_v2, item 3).

| engine | plan caching | evidence |
|---|---|---|
| DuckDB | Not in the research. | — |
| Umbra / CedarDB | Not in the research. | — |
| PostgreSQL | Custom plans for five executions, then generic if not significantly costlier; `plan_cache_mode` overrides. | (f35, A) |
| SQL Server | PSP optimization: several plan variants per query. Batch-mode adaptive joins adapt inside one cached plan (section 5). | (f35, A; f32, A) |
| CockroachDB | Custom then generic, as PostgreSQL. | (f35, A) |
| ClickHouse | Not in the research. | — |
| Calcite | Not in the research. | — |

Amphora plans once at prepare and never re-plans for a parameter value (f8). It holds SQLite's
interface but not SQLite's recompile-on-bind rule.

## Engine by topic

Each engine's coverage across the six topics, by finding. A dash means the research holds nothing
for that engine on that topic.

| engine | estimation | join ordering | unnesting | cost models | robust/adaptive | plan caching |
|---|---|---|---|---|---|---|
| DuckDB | f15, f16, f39 | f22, f24, f28 | f30, f31, f38 | f12 | f33 | — |
| Umbra / CedarDB | f14, f41 | f23 | f31, f38 | — | f33 | — |
| PostgreSQL | f11, f16, f17 | f26 | f38 | f12 | f32 | f35 |
| SQL Server | f17, f43 | — | — | — | f32 | f35 |
| CockroachDB | f16, f21 | f25 | — | — | — | f35 |
| ClickHouse | f42 | f22, f25, f27 | — | — | — | — |
| Calcite | — | f29 | — | f29 | — | — |

The research's general findings, which name no single engine: f13 (sampling, with Oracle), f18
(learned estimators), f19 and f40 (pessimistic bounds), f20 (q-error and P-error), f21 (CERT), f34
(re-optimization).

## 7. The audit: Amphora's planner against the research

What Amphora's planner does today, row by row, against what the sources found and the leading
engines do, each gap shown on a release build. The code is cited at commit f734df4, whose engine
(`crates/`) is dbef789's unchanged; the demonstrations ran on adacachy on 2026-10-25, on
amphora-bench built in release (`bench/Cargo.toml`'s profile) from f734df4. Each demonstration is
a script in `bench/planner/audit/` (`NN-*.sql`) run by `amphora-bench --eqp` (`bench/src/eqp.rs`:
a new database each script, each statement's rows printed, `-- counted` to print each table
access's estimate beside the rows it counted, `-- keep` and `-- rerun` to run a statement prepared
earlier again), its output kept beside it (`NN-*.out`). `bench/planner/audit/run.sh BINARY
--check` reruns them all and the replay of the oracles' CERT findings and compares each with the
output kept (planning times left out); it passed on f734df4. Two outputs need what run.sh
doesn't hold and say how they were made in their scripts: `tpcds-subqueries.out`
(`tpcds_subqueries.sh`, reading TPC-DS's queries from turso's perf/tpc-ds at fbcfa43) and
`panics.out` (`panics.py`, over the count pass of scripts/check-production.sh). Without ANALYZE every table is
estimated at 1,000,000 rows, so the demonstrations without data show the fallback estimates.

Which benchmark queries a row touches comes from the queries' text, in
`bench/planner/audit/benchmarks.tsv` (`features.py`: each query's most tables in one FROM clause,
its join kinds and its subqueries, correlated or not): JOB's 113 queries (gregrahn/
join-order-benchmark at a396036), TPC-H's 22 (`bench/planner/tpch/queries`), TPC-DS's 99 (turso's
`perf/tpc-ds` at fbcfa43), CH-benCHmark's 22 (benchbase at 33c0047) and ClickBench's 43
(ClickBench's `sqlite/queries.sql` at 2e258dcb). SSB's 13 queries are read from ClickHouse's
listing of their joined form (clickhouse.com/docs/getting-started/example-datasets/star-schema,
fetched 2026-10-25: Q1.1 to Q1.3 "FROM lineorder, date"; Q2.x add part and supplier, Q3.x
customer and supplier, Q4.x all four), citing "Star Schema Benchmark (O'Neil et. al), 2009". In
short: JOB's queries join 4 to 17 tables (3 of 4, 20 of 5, 21 of 8, 11 of 12, 6 of 14 and 3 of 17),
inner joins only, no subqueries; TPC-H's join at most 8; TPC-DS's at most 18 (Q64), with LEFT
JOINs in 11 queries and FULL JOINs in Q51 and Q97; SSB's at most 5; CH-benCHmark's at most 9
(Q8); ClickBench's read one table.

| # | gap | Amphora's code (f734df4) | findings (kiln grade) | demonstration |
|---|---|---|---|---|
| 1 | left-deep trees only | join.rs 1598, 1617 | f1 A, f22 A, f26 A, f27 A | 01-left-deep-greedy |
| 2 | above 12 tables a greedy left-deep search, called GOO | join.rs 1432, 1854-1858, 1940, 1994 | f1 A, f23 A, f24 A, f27 A, f28 A | 01-left-deep-greedy |
| 3 | subsets enumerated by size, not as connected pairs | join.rs 1598-1599, 1634, 2523 | f1 A, f22 A, f23 A, f10 A | 02-subsets-by-size |
| 4 | no conflict detector: RIGHT and FULL JOIN barriers, outer, semi and anti joins held behind every table before them | join.rs 2026-2052 | f9 A, f25 A | 03-right-full-barrier |
| 5 | statistics only sqlite_stat1; 1,000,000 rows and fixed selectivities without ANALYZE; filters multiplied as independent | stats.rs 10-42; cost_params.rs 106-119; cost.rs 357, 408-458; constraints.rs 486-523 | f2 A, f3 A, f11 A, f13 D, f14 A, f15 A, f17 A, f39 A, f41 A, f42 A, f43 A | 04-statistics |
| 6 | a join's estimate from the access path chosen, not from the set of tables joined (AG) | join.rs 216-264; access_method.rs 1116, 1578, 1917; cost.rs 457 | f4 A, f5 B, f6 B, f15 A, f16 A, f21 A | 05-estimate-by-path, cert-replay |
| 7 | a FULL JOIN's estimate leaves out the right table's unmatched rows | join.rs 282-329; access_method.rs 1578-1584 | f5 B, f16 A, f21 A | 06-full-join-estimate |
| 8 | a join on columns no index leads takes a fixed 0.1, with no key or containment | constraints.rs 515-517; cost_params.rs 110 | f15 A, f39 A, f43 A, f11 A | 11-join-size |
| 9 | a GROUP BY's groups are not estimated | eqp.rs 950-960 | f14 A | 11-join-size |
| 10 | unnesting by fixed patterns | unnest.rs 1-7, 106, 154-158, 1267; mod.rs 999-1035 | f7 A, f30 B, f31 A, f38 C, f10 A | 07-unnesting, 13-tpch-subqueries, tpcds-subqueries |
| 11 | plans fixed at prepare: no re-plan for new statistics or a parameter's value | statement.rs 709-735, 751, 1040, 1453 | f8 B, f35 A | 08-fixed-at-prepare |
| 12 | a page-based cost model whose pages are rows over 50 | cost.rs 121-134, 187; cost_params.rs 107 | f9 A, f12 D | 09-cost-model |
| 13 | no adaptive or robust execution | join.rs 70, 1024; statement.rs 1040 | f32 A, f33 A, f34 A, f11 A | 10-no-adaptive |
| 14 | no upper bound on an estimate | join.rs 259-264; cost.rs 408-458 | f19 A, f40 C, f11 A | 04-statistics, 10-no-adaptive |
| 15 | estimation error measured per operator; its q-error baseline over JOB and TPC-H is section 8's, P-error not yet measured | statement.rs 495; connection.rs 3969 | f36 B, f20 A, f37 A | the `-- counted` lines of 04, 05, 06, 08, 10, 11; section 8 |
| 16 | the greedy search, and the planner around it, hold unwrap, expect and panics | join.rs 1967, 2020, 2114; panics.out | f10 A | panics.out, 01-left-deep-greedy |

### Row 1. Left-deep trees only

- **Amphora.** Join orders are searched by a dynamic program over table subsets, size by size,
  each subset built from a subset one table smaller and one more table (join.rs line 1598: "for
  subset_size in 2..=num_tables {"; line 1617: "In this block, LHS is always (n-1) tables, and RHS
  is a single table."). Every plan is a loop nest, one table a level.
- **The leading engines.** PostgreSQL searches bushy trees without cross products exhaustively
  up to its collapse limits (f26, A); DuckDB enumerates bushy plans by DPhyp (f24, A); ClickHouse
  by DPsub and CockroachDB by DPSube (f25, A); Calcite by a Volcano dynamic program over
  rule-made alternatives (f29, A). On JOB with true cardinalities, left-deep trees cost up to 4.5
  times the best bushy plan (f27, A).
- **The gap.** No plan joins two composite inputs: `(a JOIN b) JOIN (c JOIN d)` runs as one
  pipeline, a table a step (`01-left-deep-greedy.out`, the last plan: "HASH JOIN b", "SEARCH c
  USING COVERING INDEX ephemeral_c_t3 (y=?)", "SEARCH d USING COVERING INDEX ephemeral_d_t4
  (z=?)", "SCAN a"). A hash join's build is one table (join.rs line 1333, the build's mask
  "[build_table_idx]"), never a join's result.
- **Benchmarks touched.** Every multi-join query: JOB's 113; TPC-H Q2, Q3, Q5, Q7 to Q10, Q18,
  Q21 (3 to 8 tables); TPC-DS Q64 (18 tables), Q72 (11), Q17, Q25, Q29, Q85 and Q88 (8) and the
  rest of its joins; SSB Q2.1 to Q4.3 (4 and 5 tables); CH-benCHmark Q2, Q5, Q7, Q8 and Q9 (6 to
  9). ClickBench reads one table and is not touched.

### Row 2. Above 12 tables, a greedy left-deep search called GOO

- **Amphora.** Above 12 tables (join.rs line 1854: "pub const GREEDY_JOIN_THRESHOLD: usize =
  12;"; line 1432: "if num_tables > GREEDY_JOIN_THRESHOLD {") the search is "Greedy Operator
  Ordering (GOO)" in name (line 1856), but it "Builds a left-deep join tree by:" (line 1858)
  adding, one at a time, the table of least cost at that step (line 1940: "Greedily add remaining
  tables, always picking lowest marginal cost."; line 1994: "if best.as_ref().is_none_or(|(_, b)|
  plan.cost < b.cost) {"). The cost compared is the plan's so far: a table's filter, which cuts
  the rows every later step reads, counts for nothing at the step it is added.
- **The leading engines.** GOO as published keeps a set of trees and merges the pair of least
  cost, so it builds bushy trees (f28, A). DuckDB runs DPhyp until 10,000 pairs, then GOO (f24,
  A). Neumann and Radke solve exactly within a budget of 10,000 connected subgraphs up to 100
  relations and linearize beyond (f23, A). On JOB, GOO's plans cost up to 21 times the optimum
  with true cardinalities (f27, A).
- **The gap.** A fact table and twelve dimensions, the last filtered (`01-left-deep-greedy.out`):
  with eleven (12 tables) the dynamic program reads the filtered dimension right after the fact
  table, total_cost 6,449,003.3; with twelve (13 tables) the greedy search reads it last, total_cost
  38,579,003.6, where the same 13 tables forced into the dynamic program's order cost 6,770,303.6:
  5.7 times more by Amphora's own model.
- **Benchmarks touched.** JOB 28a to 28c and 33a to 33c (14 tables) and 29a to 29c (17); TPC-DS
  Q64 (18). No query of TPC-H, SSB, CH-benCHmark or ClickBench joins more than 12 tables.

### Row 3. Subsets enumerated by size, not as connected pairs

- **Amphora.** The dynamic program walks every subset of each size (join.rs line 1599: "for mask
  in generate_join_bitmasks(num_tables, subset_size) {"; line 2523), and only then skips a
  join that isn't connected where a connected one exists (line 1634: "if
  has_connected_legal_candidate("). The subsets a chain of n tables can join without a cross
  product are n(n+1)/2 (78 for 12); the walk visits all 2^n - 1 (4,095).
- **The leading engines.** DPccp and DPhyp enumerate connected complement pairs only and meet the
  lower bound, where DPsize runs far above it (f22, A); DuckDB, ClickHouse and CockroachDB do
  (f24, f25, A). As published DPhyp is recursive; Amphora's rules require it as a loop over an
  explicit stack with a counted budget (f10, A).
- **The gap.** Planning time (`02-subsets-by-size.out`, the least of five prepares of EXPLAIN
  QUERY PLAN, on adacachy at a load average near 70 from other builds, so a diagnosis, not a
  benchmark record): a chain of 8 to 12 tables 0.097, 0.146, 0.247, 0.383 and 0.682 ms, 7.0 times
  from 8 to 12 where its connected subsets grow 2.2 times (36 to 78); a star of 8 to 12 tables
  0.802, 2.019, 5.875, 15.342 and 38.508 ms, 48 times. The search can't stay exact past 12 tables
  within a budget, which is why row 2's greedy search starts there.
- **Benchmarks touched.** Planning time of JOB's 28 queries of 10 to 12 tables and TPC-DS Q72
  (11); with a connected-pair search, the 9 JOB queries and TPC-DS Q64 above 12 could be planned
  exactly too.

### Row 4. No conflict detector

- **Amphora.** A LEFT or FULL JOIN's right table, and a semi- or anti-join's (EXISTS, NOT
  EXISTS, IN unnested), must follow every table written before it (join.rs lines 2029-2038: "LEFT/
  FULL OUTER, SEMI, and ANTI joins all require the RHS table"); a RIGHT or FULL JOIN also keeps
  every later table after its right table (line 2041: "A RIGHT or FULL JOIN is a reordering
  barrier in both directions.").
- **The leading engines.** ClickHouse and CockroachDB reorder outer, semi and anti joins exactly
  with Moerkotte, Fender and Eich's CD-C, "correct and complete" (f25, A); DPhyp handles non-inner
  joins in its hypergraph (f22, A).
- **The gap.** `(big RIGHT JOIN mid ON mid.k = big.k) JOIN tiny ON tiny.id = mid.id WHERE tiny.id
  = 5` equals `big RIGHT JOIN (mid JOIN tiny)`, since tiny's term reads only mid
  (`03-right-full-barrier.out`): with inner joins tiny's one row is read first, total_cost 23,007.0;
  with the RIGHT JOIN big and mid are read first, total_cost 328,300,039,000.3; the same query
  written as `mid JOIN tiny ... LEFT JOIN big` reads tiny first again, 23,007.0.
- **Benchmarks touched.** TPC-DS Q51 and Q97 (FULL JOINs of two inputs, where the barrier costs
  nothing) and the LEFT JOINs of TPC-H Q13, CH-benCHmark Q13 and TPC-DS Q5, Q40, Q49, Q72, Q75,
  Q77, Q78, Q80 and Q93; the semi- and anti-joins unnesting makes (row 10: TPC-H Q4 and Q22).
  JOB, SSB and ClickBench have none.

### Row 5. Statistics: sqlite_stat1 only, fallbacks without ANALYZE, independence

- **Amphora.** Statistics are `sqlite_stat1`'s (stats.rs line 10: "pub const STATS_TABLE: &str =
  "sqlite_stat1";"): a table's rows and, per index, the "Average number of rows per distinct key
  prefix, for each leftmost prefix" (line 18). No distinct count for a column no index leads, no
  histogram, no sample, no sketch. Without ANALYZE a table has "rows_per_table_fallback:
  1_000_000.0," (cost_params.rs line 106; cost.rs line 357), and filters fixed selectivities
  (cost_params.rs lines 110-119: an equality on a column no index leads 0.1, on an indexed one
  0.001, a range 0.4, a closed range 0.005, LIKE 0.2); after ANALYZE an equality on a column no
  index leads stays at 0.1 (constraints.rs lines 515-517: "params.sel_eq_unindexed"). A table's
  filters multiply (cost.rs line 429: "fraction *= constraint.selectivity;"), but for a closed
  range on one column (lines 448-449).
- **The leading engines.** Umbra and CedarDB keep a reservoir sample of every relation, updated
  online, and HyperLogLog sketches on every column (f14, f41, A); DuckDB HyperLogLog over a sample
  of each vector and join sizes from equivalence-class domains (f15, f39, A); ClickHouse t-digest,
  count-min and HyperLogLog per column, built by default (f42, A); PostgreSQL extended statistics
  by hand and SQL Server exponential backoff across predicates (f17, A). Samples catch
  correlations per-column statistics miss (f13, D, against Han et al.'s finding that sampling
  over joins did worse than PostgreSQL). On JOB every estimator erred by 1,000 times or more,
  multi-join results underestimated under independence (f11, A).
- **The gap.** On 10,000 rows (`04-statistics.out`): before ANALYZE, "output_rows": 1000000;
  after it, a = 1 AND b = 1 on two equal columns is estimated at 1 row and counted 100; flag = 1
  (two values, no index) estimated 1,000, counted 5,000; skew = 0 (9,900 rows) and skew = 9950
  (1 row) both estimated 100; u > 20000 estimated 4,000, counted 0.
- **Benchmarks touched.** Every query. Correlated predicates: JOB's join-crossing correlations
  (f13), TPC-H Q19's OR of conjunctions and Q6's three ranges, SSB Q1.1 to Q1.3 (ranges on
  lo_discount, lo_quantity and the date); ClickBench's filters on hits (its 43 queries read one
  table, most filter it, 29 group it); skew: ClickBench's columns and TPC-DS's (date_dim, item).

### Row 6. A join's estimate from the access path chosen (the open AG findings)

- **Amphora.** A join's rows are the input's times the access path's rows per outer row times its
  filters (join.rs lines 259-264: "input_cardinality * method.estimated_rows_per_outer_row *
  remaining_filter_selectivity * completed_selectivity"): an index seek's rows per outer row are
  its seek's (access_method.rs line 1116: "estimated_rows_per_outer_row: rows_per_seek,", from
  sqlite_stat1 by cost.rs line 457's estimate_rows_per_seek), a hash join's from its keys
  (access_method.rs line 1578), a scan's the table's rows (line 1917) with its terms then taken
  as filters at their fixed selectivities. The AF fix (f4) made a term's estimate independent of the
  order tables are read in; the estimate still depends on how a table is reached.
- **The leading engines.** PostgreSQL keeps one rowcount for every path of a join relation,
  CockroachDB gives a memo group's expressions one row count, DuckDB caches a cardinality per
  relation set (f16, f15, A). CERT checks a more restrictive query is never estimated at more
  rows (f21, A).
- **The gap.** r JOIN s ON s.k = r.k (2,000 rows each, 20,000 matches; `05-estimate-by-path.out`):
  through s's index on k, estimated 20,000 and counted 20,000; with s NOT INDEXED, 400,000; read s
  first by CROSS JOIN, 400,000. Campaign 2's cert-70d3e0baf8df5914 still fails: 10,000 rows, then
  320,000 with a conjunct added. Replaying every CERT finding the oracles recorded on f734df4
  (`cert-replay.out`, `cert_replay.py`, the oracle's own estimate rule): AF's 35 and P1 to P3's 4
  pass; of AG's 87, 82 fail, 4 have no estimate, 1 passes. Of the 82, 39 make an outer join inner
  (16 FULL, 13 RIGHT, 10 LEFT) and 43 add a conjunct (22 to inner joins alone, 17 beside a
  subquery, 4 to outer joins).
- **Benchmarks touched.** Every join: JOB's 113, TPC-H's 18 joining queries, TPC-DS's, SSB's
  13, CH-benCHmark's.

### Row 7. A FULL JOIN's estimate leaves out the right table's unmatched rows

- **Amphora.** An outer join's rows per input are the matches plus a Poisson chance of none
  (join.rs lines 286-288: "A Poisson model estimates the chance that an input row has no match.");
  that counts the left input's unmatched rows. Nothing adds the right table's rows that matched
  no left row, which a RIGHT or FULL JOIN emits in its pass after the loops. A hash FULL JOIN
  takes at least the probe's rows over the build's (access_method.rs lines 1582-1584).
- **The leading engines.** A FULL JOIN is never below either side's outer join (CERT's
  restriction monotonicity, f21, A); the set-based estimators give each join relation one row
  count (f16, A).
- **The gap.** l (100 rows) and r (1,000), no key matching (`06-full-join-estimate.out`): LEFT
  JOIN counted 100, RIGHT JOIN 1,000, FULL JOIN 1,100, each estimated 136.79; the inner join 100.
  The open AG findings that make a FULL JOIN inner (16 above) are this row's.
- **Benchmarks touched.** TPC-DS Q51 and Q97; no other benchmark has a RIGHT or FULL JOIN.

### Row 8. Join sizes: a fixed selectivity on columns no index leads

- **Amphora.** An equality on a column no index leads, a join term among them, is 0.1
  (constraints.rs lines 515-517; cost_params.rs line 110: "sel_eq_unindexed: 0.1,") with or
  without ANALYZE: a key column not declared one, and a join of two foreign keys, has no
  distinct count to divide by.
- **The leading engines.** DuckDB and Ebergen's thesis take every join as foreign key to primary
  key and divide by the equivalence class's domain, its largest distinct count (f15, f39, A); SQL
  Server assumes containment (f43, A).
- **The gap.** o (5,000 rows) JOIN c (500) ON c.ck = o.ck, ck unique in c but no index
  (`11-join-size.out`): estimated 250,000, counted 5,000.
- **Benchmarks touched.** Every join whose columns are loaded without an index: TPC-DS's as
  turso's schema declares it (no key and no index on any of its tables), SSB's star joins
  unless its keys are declared. The planner bench indexes every foreign key of TPC-H
  (`tpch/indexes.sql`) and JOB (`fkindexes.sql`), so there a join reaches row 6's seek
  estimates instead, and this row returns wherever the search picks a hash join or a scan.

### Row 9. A GROUP BY's groups are not estimated

- **Amphora.** A query's estimate is its join's last access (eqp.rs lines 950-960: "The rows the
  join is estimated to give"); grouping, DISTINCT and a derived table's groups are not estimated,
  so a grouped subquery joined to another table enters the join at its input's rows.
- **The leading engines.** CedarDB says its statistics "estimate relation size after group by
  operations" (f14, A).
- **The gap.** o's 5,000 rows grouped by ck (500 values): estimated 5,000 (`11-join-size.out`).
- **Benchmarks touched.** Where a group's result is joined or filtered: TPC-H Q13 (a derived
  table of counts), Q15 (a WITH of revenue), Q18 (IN over GROUP BY ... HAVING); TPC-DS's CTEs of
  aggregates joined to tables (Q1, Q30, Q81 among them, 82 of its queries group); ClickBench's 29
  GROUP BY queries and SSB's Q2 to Q4 for the sort after the groups.

### Row 10. Unnesting by fixed patterns

- **Amphora.** EXISTS and NOT EXISTS become semi- and anti-joins, a direct positive IN a
  semi-join, and a one-aggregate subquery is grouped by its correlation columns (unnest.rs lines
  1-7); "`NOT IN` stays as a subquery because NULL values can change its result." (line 106);
  nothing is unnested beside a RIGHT or FULL JOIN (lines 154-158); a semi-join's subquery must
  read one table (line 1267). The unnested form is kept only when estimated no costlier than the
  written one (mod.rs lines 1021-1035), each planned on its own ("Until then, both forms need
  their own search.", line 1001).
- **The leading engines.** Neumann and Kemper unnest every correlated subquery by dependent joins
  (f30, B); DuckDB uses it, CedarDB decorrelates fully (f31, A); Neumann's 2025 top-down variant,
  in Umbra, visits each operator once and turned PostgreSQL's 26 minutes on its Figure 1 query
  into 1.3 s (f38, C). As published both are recursive; Amphora would need them as a worklist
  loop (f10, A).
- **The gap.** Unnested (`13-tpch-subqueries.out`, TPC-H's own text and schema): Q4 (semi), Q22's
  NOT EXISTS (anti), Q2 and Q17 (grouped first), Q20's correlated sum (joined, then grouped).
  Kept: Q21's EXISTS and NOT EXISTS over its four outer tables, estimated cheaper as written (over
  two, `07-unnesting.out`, they are unnested). Uncorrelated, run once: Q11 and Q15's scalars, Q16,
  Q18 and Q20's IN lists. On TPC-DS (`tpcds-subqueries.out`): Q1, Q6, Q30, Q41 and Q81 keep a
  "CORRELATED SCALAR SUBQUERY", Q10, Q35 and Q69's EXISTS read two tables each and stay
  subqueries, Q16 and Q94's stay on cost; Q32 and Q92 are grouped first. Beside a RIGHT JOIN, Q4's
  EXISTS is a subquery (`07-unnesting.out`, no "join": "semi"); an OR of two EXISTS scans orders
  and lineitem for every customer; a correlated NOT IN is a "CORRELATED LIST SUBQUERY".
- **Benchmarks touched.** Kept: TPC-H Q21; TPC-DS Q1, Q6, Q10, Q16, Q30, Q35, Q41, Q69, Q81,
  Q94. Under the same rules, not run here: CH-benCHmark Q4, Q21 and Q22 (correlated EXISTS and
  NOT EXISTS, as TPC-H's) and Q16 (NOT IN). JOB, SSB and ClickBench have no subqueries.

### Row 11. Plans fixed at prepare

- **Amphora.** A statement is planned at prepare and again only when its run meets a schema
  change (statement.rs lines 709-735, `SchemaUpdated` to `reprepare`, line 1040). ANALYZE refreshes
  the connection's statistics (line 751) but re-plans no statement already prepared; a bound
  value is stored for the run (line 1453), never planned with.
- **The leading engines.** PostgreSQL and CockroachDB plan a prepared statement with its values
  for five runs, then generic if not significantly costlier; SQL Server keeps several plans by the
  parameters' cardinality; SQLite recompiles when a bound value could change the plan (f35, A).
- **The gap.** `08-fixed-at-prepare.out`: a join prepared before ANALYZE and run again after it
  keeps "output_rows": 100000000000 and its plan; the same text prepared after it, 5000. `WHERE k
  = ?` has one plan and one estimate, 1,000 rows, where k = 1 counts 1,000 and k = 9 none.
- **Benchmarks touched.** CH-benCHmark (its transactions prepared once while the analytical
  queries' tables grow under them), TPC-H and TPC-DS when their streams run prepared with
  substitution parameters; JOB, SSB and ClickBench run literal queries once.

### Row 12. A page-based cost model whose pages are rows over 50

- **Amphora.** Cost counts page reads, seeks, row, filter, hash and sort work in one unit (f9),
  table lookups by Mackert and Lohman (cost.rs line 187); a table's pages are its rows over a
  fixed 50 (cost.rs line 126: "let table_pages = (base_row_count /
  params.rows_per_table_page).max(1.0);"; cost_params.rs line 107: "rows_per_table_page: 50.0,"),
  not the pages it holds.
- **The leading engines.** On JOB, cost-model errors were "dwarfed by cardinality estimation
  errors"; DuckDB's join cost is each join's cardinality plus its inputs' (f12, D, graded so for
  Amphora's own counterclaim: its bench's plans did more work from cost gaps, fixed in planner.md
  decisions 2, 5 and 7). Calcite combines CPU, I/O and memory (f29, A).
- **The gap.** Two tables of 2,000 rows, one narrow, one of 2,000-byte rows
  (`09-cost-model.out`): the file grows from 8 pages to 1,011, both scans cost 46.
- **Benchmarks touched.** Wide rows: ClickBench's hits, TPC-DS's fact tables, JOB's text tables
  (movie_info, person_info), where a scan's cost is under-counted against a seek's.

### Row 13. No adaptive or robust execution

- **Amphora.** A plan runs as prepared (statement.rs line 1040 is the only path that plans
  again): no switch between nested loops and a hash join at run time, no re-optimization on rows
  counted, no Bloom filter passed between joins; a hash join is ruled out when its estimated
  build passes 200,000 rows (join.rs line 70: "const MAX_MATERIALIZED_BUILD_ROWS: f64 =
  200_000.0;", line 1024).
- **The leading engines.** SQL Server's adaptive join picks hash or nested loops after its first
  input against a threshold; on JOB, removing nested loops without an index ended the timeouts
  (f32, A). LIP, predicate transfer, POLAR and diamond-hardened joins make plans robust to join
  order (f33, A); mid-query re-optimization cut JOB's 20 slowest queries by 27% (f34, A).
- **The gap.** p and q analyzed at 10 rows, then grown to 3,000 (`10-no-adaptive.out`): estimated
  10 and 10, counted 3,000 and 3,000, the plan "SCAN p" with "SCAN q" inside it, a full scan of q
  for every row of p.
- **Benchmarks touched.** JOB (estimates off by 1,000 times, f11); star joins where Bloom filters
  pay: SSB Q2.1 to Q4.3, TPC-H Q5, Q8 and Q9, TPC-DS's store_sales and catalog_sales stars;
  CH-benCHmark, whose data grows past its statistics while it runs.

### Row 14. No upper bound on an estimate

- **Amphora.** Selectivities multiply without a bound (join.rs lines 259-264; cost.rs lines
  408-458): no estimate is capped by what statistics guarantee.
- **The leading engines.** LpBound and SafeBound bound join outputs from degree sequences and
  never underestimate; with several predicates on a table SafeBound takes the most selective
  predicate's cardinality (f19, A; f40, C).
- **The gap.** a = 1 AND b = 1 (`04-statistics.out`): each index guarantees 100 rows a value, the
  bound SafeBound would give, and the estimate is 1 against 100 counted; row 13's join is
  estimated 10 against 3,000.
- **Benchmarks touched.** JOB (multi-join underestimates, f11); TPC-H, TPC-DS and SSB's
  multi-predicate dimension filters.

### Row 15. Estimation error: counted per operator, its baseline in section 8

- **Amphora.** Since 016d245 a statement counts the rows at each table access and join when asked
  (statement.rs line 495: "pub fn row_counts(&self)"; connection.rs line 3969: "pub fn
  set_row_counters"), closing what f36 found missing. Step 3 recorded the q-error at every join
  prefix of JOB and TPC-H (section 8); P-error is not measured yet.
- **The leading engines and sources.** q-error bounds a plan's cost; P-error tracks end-to-end
  time better (f20, A); the workloads are at hand (f37, A).
- **The gap.** Every `-- counted` line of the demonstrations is an estimate beside its count
  (04, 05, 06, 08, 10, 11), and section 8 has the benchmark-wide distribution; what it measures is
  rows 5, 6, 8 and 14's error, and no plan's cost yet (P-error).
- **Benchmarks touched.** JOB's 113 and TPC-H's 22 (step 3); the others once they are loaded.

### Row 16. Panicking calls in the greedy search and the planner

- **Amphora.** CLAUDE.md section 1 forbids unwrap, expect, panic and unreachable in production
  code (f10, A). The greedy search holds `join_order.last_mut().unwrap()` (join.rs line 1967),
  `current_plan.expect("loop invariant: current_plan always Some")` (line 2020) and, in its
  starting table, `best.expect("no valid starting table")` (line 2114); the join search beside it
  two unwraps (lines 612, 695), and its hash join an expect (1333) and an unreachable (1003). The planner's files
  (`panics.out`, from scripts/check-production.sh's count pass on f734df4) hold 26 unwrap, 54
  expect, 8 panic, 10 unreachable, 253 slice indexings and 67 unchecked arithmetic: join.rs 3, 3,
  0, 1, 47, 14; mod.rs 7, 11, 5, 3, 89, 13; constraints.rs 9, 7, 3, 1, 39, 3; unnest.rs 0, 6, 0,
  3, 15, 6; statement.rs 2, 14, 0, 0, 3, 2; stats.rs 1, 0, 0, 0, 4, 6.
- **The leading engines.** Not in the research; the bar is Amphora's own.
- **The gap.** Any change to the search, the estimator or the unnesting replaces the calls it
  touches with typed errors, and lowers scripts/engine-ratchet.toml with them.
- **Benchmarks touched.** Every query is planned through mod.rs and join.rs; the greedy search's
  three calls run for JOB 28a to 29c and 33a to 33c and TPC-DS Q64.

### The research with no row

- **Learned estimators** (f18, D): more accurate on static data, costly to train and to keep with
  updates, and Han et al. find them improving PostgreSQL where traditional methods hardly do; no
  gap is claimed until a measurement on Amphora's workloads says which.
- **Calcite's rule engines** (f29, A): a framework's design, not a technique Amphora lacks for
  these benchmarks.
- **The rules on how a change is built** (f10, A): no recursion, counted budgets; they bind rows
  1 to 3 and 10 (DPhyp and both unnesting papers are stated recursively).

## 8. The baseline: Amphora's estimates against the rows counted, JOB and TPC-H

Step 3's measurement (`bench/records/2026-10-26-jupiter-planner-baseline.md`, its records beside it;
`bench/planner/README.md` says how it is made): amphora-bench in release from 26657fd, whose engine
is section 7's (f734df4's `crates/`), on jupiter (AMD Ryzen AI Max+ 395, 32 threads, 123.6 GiB,
NTFS, Windows 11 Pro 10.0.26200), 2026-10-26 and 27. Each benchmark was loaded after ANALYZE, and
every query run three times on each engine and once more with Amphora's row counters, each run begun
at a load under 2. Every one of JOB's 113 queries and TPC-H's 22, at scale factors 1 and 10, has
Amphora's estimate and the rows it counted at every join prefix (977 prefixes on JOB, 94 at each
scale factor of TPC-H, subqueries' plans included); none was refused, and every pass recorded the
same estimates and counts.

**q-error by the joins in the prefix** (max(est/act, act/est), both at least one row, Moerkotte et
al.; nearest-rank percentiles; a prefix with n joins is the (n+1)th table of its plan's join order):

| joins in the prefix | prefixes | median q-error | p90 | p95 | max |
|---:|---:|---:|---:|---:|---:|
| 0 | 113 | 1 | 1.342e+04 | 1.342e+04 | 1.924e+04 |
| 1 | 113 | 10.9 | 365 | 1.380e+04 | 1.866e+05 |
| 2 | 113 | 37.9 | 3.45e+03 | 1.237e+05 | 5.031e+05 |
| 3 | 113 | 1.07e+03 | 5.638e+05 | 7.855e+05 | 9.048e+06 |
| 4 | 110 | 1.14e+03 | 2.684e+05 | 1.302e+06 | 7.797e+06 |
| 5 | 90 | 4.69e+03 | 9.783e+05 | 4.961e+06 | 7.797e+06 |
| 6 | 88 | 3.44e+03 | 2.216e+05 | 8.815e+05 | 2.833e+06 |
| 7 | 72 | 2.97e+03 | 3.199e+05 | 8.372e+05 | 4.317e+06 |
| 8 | 51 | 2.5e+03 | 5.328e+05 | 9.339e+05 | 4.112e+07 |
| 9 | 37 | 1.56e+03 | 9.339e+05 | 1.268e+07 | 4.112e+07 |
| 10 | 30 | 743 | 4.502e+04 | 5.895e+04 | 1.897e+05 |
| 11 | 20 | 477 | 8.65e+03 | 1.146e+04 | 1.501e+04 |
| 12 | 9 | 6.14e+03 | 1.151e+06 | 1.151e+06 | 1.151e+06 |
| 13 | 9 | 1.62e+03 | 7.676e+05 | 7.676e+05 | 7.676e+05 |
| 14 | 3 | 30 | 803 | 803 | 803 |
| 15 | 3 | 4.89e+03 | 9.044e+04 | 9.044e+04 | 9.044e+04 |
| 16 | 3 | 1.62e+03 | 2.240e+04 | 2.240e+04 | 2.240e+04 |
| all | 977 | 273 | 1.651e+05 | 7.384e+05 | 4.112e+07 |

| joins in the prefix | prefixes | SF 1 median | p90 | p95 | max | SF 10 median | p90 | p95 | max |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | 41 | 9.4 | 2.5e+03 | 2.054e+04 | 4.991e+04 | 8.08 | 1e+04 | 1.999e+05 | 3.108e+05 |
| 1 | 22 | 9.4 | 2.5e+03 | 3.099e+04 | 1.183e+05 | 19.1 | 1e+04 | 3.108e+05 | 1.187e+06 |
| 2 | 12 | 15.3 | 2.5e+03 | 4.991e+04 | 4.991e+04 | 3.68 | 60.9 | 2.5e+03 | 2.5e+03 |
| 3 | 8 | 60.6 | 4.991e+04 | 4.991e+04 | 4.991e+04 | 38.2 | 1e+03 | 1e+03 | 1e+03 |
| 4 | 5 | 60.6 | 4.991e+04 | 4.991e+04 | 4.991e+04 | 60.9 | 121 | 121 | 121 |
| 5 | 4 | 30.8 | 4.991e+04 | 4.991e+04 | 4.991e+04 | 8.08 | 1.52e+03 | 1.52e+03 | 1.52e+03 |
| 6 | 1 | 6.94 | 6.94 | 6.94 | 6.94 | 8.08 | 8.08 | 8.08 | 8.08 |
| 7 | 1 | 6.94 | 6.94 | 6.94 | 6.94 | 8.08 | 8.08 | 8.08 | 8.08 |
| all | 94 | 18.8 | 2.004e+04 | 4.991e+04 | 1.183e+05 | 9.39 | 2.5e+03 | 1.999e+05 | 1.187e+06 |

**The largest intermediate result** (each query's largest counted result, the one Han et al. find
matters; f20):

| benchmark | queries | median | p90 | p95 | max |
|---|---:|---:|---:|---:|---:|
| JOB | 113 | 322 | 2.833e+06 | 7.797e+06 | 4.112e+07 |
| TPC-H SF 1 | 22 | 30.3 | 3.099e+04 | 4.991e+04 | 1.183e+05 |
| TPC-H SF 10 | 22 | 7.65 | 1.999e+05 | 3.108e+05 | 1.187e+06 |

**What it shows.** On one table JOB's estimates are mostly right (median 1), but 16 of the 113 are
off by 13,417 or more, 15 of them an equality on `keyword.keyword`, which no index covers: row 5's
fixed selectivity of 0.1 (cost_params.rs line 110) holds whatever ANALYZE found, and 1 row counted
is estimated at 13,417, a tenth of keyword's 134,170. Each join then multiplies the error on (rows
5, 6, 8 and 14): the median passes 1,000 by the third join, and down the long chains the estimate
falls below one row while the counts grow, so the q-error there is the count itself (26c's ninth
table: 3.4e-33 rows estimated, 41,119,579 counted). TPC-H's joins follow keys and its error stays
nearer (median 18.8 over every prefix at SF 1, 9.39 at SF 10); its largest come from filters' fixed
selectivities (at SF 10, q16's three filters on part leave 1.44 rows estimated against 296,824
counted, and its join to partsupp 0.0576 against 1,186,602). Amphora's planning time is a median 1.9
ms on JOB (92.1 ms at most, 27c's 12 tables) and 0.5 to 0.6 ms on TPC-H.

**Run times** (best of 3; each engine's time over Amphora's, geometric mean over the queries both
finished, under 1 when the engine was faster; and on how many it was the faster):

| benchmark | SQLite | DuckDB, one thread | DuckDB, 32 threads | PostgreSQL |
|---|---:|---:|---:|---:|
| JOB (113) | 1.101, 63 faster | 1.013, 64 faster | 0.435, 75 faster | 0.354, 83 faster |
| TPC-H SF 1 (22) | 1.417, 12 faster | 0.481, 16 faster | 0.126, 20 faster | 0.535, 19 faster |
| TPC-H SF 10 (22) | 1.198 (21 of 22), 13 faster | 0.289, 16 faster | 0.021, 22 faster | 0.329, 20 faster |

JOB's and SF 1's times are their second passes', timed by fresh processes on loaded data: the first
passes, timed by the process that had just loaded them, ran slower (DuckDB on all 32 threads
throughout, the others over JOB's first hour and a half), and `record.sh` now loads in a process of
its own (c652376). SF 10's are the best of its passes a, b and c, each query's fastest. PostgreSQL
ran in Docker Desktop, which stopped twice; its SF 10 and JOB times are passes of its own (JOB's
right after loading it), and it has no time in SF 10's passes b and c. Every engine counted the same
rows for every query; SQLite passed the 600 s deadline on SF 10's q07 in every pass. The record says
what each left out and why.

**The repeat** (the plan's test: two passes of one build on one machine record the same estimates,
and median run times within 5%): every pair of passes recorded the same estimates and counts (0 of
JOB's 1,090 counters and 0 of TPC-H's 135 differ, at SF 1 and at SF 10). Timed alike by fresh
processes, with the records that ran beside other work on jupiter run again (a monitor's samples and
`bench/planner/disturbed.py` name them), TPC-H SF 10's passes b and c are within 5% for every engine
(Amphora 1.006, SQLite 0.990, DuckDB 1.037, on all 32 threads 1.001), and JOB's passes b and d for
Amphora (0.973), SQLite (0.982) and DuckDB (0.959) but not DuckDB on all 32 threads (1.155 by the
median of its best times, 1.033 by the median of its queries' ratios). PostgreSQL's 51 undisturbed
queries of the 54 it ran in JOB's pass d are within 5% of its own earlier pass (0.967); Docker
Desktop stopped before its other 59 and before SF 10's passes b and c.

**The counters off** (the step's fourth criterion: the bench suite's statements with the row
counters off within 2% of the change's parent, best of 3): 9cfc8b9's record left 56 of its 104
statement and client-count pairs outside 2%. Measured again, each statement alone in ABBA turns, 35
of the 56 were within 2% on jupiter (`bench/records/2026-10-27-jupiter-row-counters-off-pairs.md`),
and the 19 statements behind the other 21 were measured again on adas-mac-studio-2
(`bench/records/2026-10-27-adas-mac-studio-2-row-counters-off-19.md`). On neither machine is a pair
slower with the counters off by more than its builds' own runs vary; five pairs are faster with them
off in every round on Studio 2 (`date_bucket`, `like_substring` and `string_functions` at 1 client,
`point_pk_one_column` and `range_secondary_100` at 32), the two binaries' other differences, since a
connection that asks for no counts runs no counting instruction. Ada judged the criterion met on
that evidence (2026-10-27).

## 9. The changes ranked

Step 6 of the plan: the planner changes sections 1 to 8 call for, ranked by their expected gain on
TPC-H, TPC-DS, JOB, ClickBench, SSB and CH-benCHmark for the work and the risk each takes, each with
the measurement that would show it and what would falsify it. Step 3 measured JOB and TPC-H (section
8). TPC-DS, ClickBench, SSB and CH-benCHmark are not loaded yet, so what a change would do to them
comes from the sources and from their queries' text (section 7's benchmark notes), not from a
measurement. Each number taken from step 3 is as
`bench/records/2026-10-26-jupiter-planner-baseline.md` takes it: JOB's from its pass b (with
`-job-at` for DuckDB's 15a to 15d and `-job-pg` for PostgreSQL), TPC-H SF 1's from its pass b, SF
10's the best of its passes a, b and c, c's reruns and PostgreSQL's own pass.
`bench/planner/cited.tsv` lists every one with its records and how it is computed;
`bench/planner/cited.py` recomputes each by `summary.py`'s definitions, and with `--job` the counts
this section takes from JOB's text (gregrahn/join-order-benchmark at a396036):
`python3 -I bench/planner/cited.py --job DIR` ends "cited.py: 84 numbers from step 3's records, 0
differ; 5 from JOB's text, 0 differ".

Step 3 measured f734df4's estimator. Decision 13 (step 4, d762dc2) and step 5 (10f2461) have changed
the estimates since, so a change is measured by `record.sh` on the change and on its parent, on one
machine, each run begun at a load under 2 (CLAUDE.md §7): step 3's numbers set the targets, and the
parent's pass says what the change itself moved. The first such pass is of this branch, before rank
1.

Step 3's records say where the gain is. Of JOB's 113 queries, the 52 whose largest intermediate
result is misestimated by 1,000 times or more (the factor every estimator routinely erred by, f11)
take 77% of Amphora's time on JOB. On them SQLite's time over Amphora's is 0.819 and DuckDB's on one
thread 0.509; on the other 61 they are 1.416 and 1.823. At TPC-H's SF 10 the 6 such queries give
SQLite 0.725, the other 15 it finished 1.465. PostgreSQL's lead is about the same on either side
(0.344 and 0.362), so it does not rest on Amphora's estimates. By the tables joined, Amphora is the
faster on JOB's 28 queries of 10 to 12 tables, which its dynamic program plans (SQLite's time over
Amphora's 1.813), and the slower on the 9 above 12, which its greedy search plans (0.405). So
estimation comes first, and the search above 12 tables soon after.

| rank | change | audit rows | JOB | TPC-H | TPC-DS | SSB | CH-benCHmark | ClickBench | work | risk |
|---:|---|---|---|---|---|---|---|---|---|---|
| 1 | join sizes from equivalence classes, with key caps | 8, 14 | large | small | large | none to large | medium | none | small | moderate |
| 2 | distinct counts and a sample per table, kept by ANALYZE | 5, 9, 11 | medium | medium | medium | medium | medium | medium | large | moderate |
| 3 | connected subsets within a counted budget | 2, 3, 16 | medium | none | small | none | none | none | medium | moderate |
| 4 | each table's filters estimated together on its sample | 5 | medium | medium | medium | medium | small | medium | medium | moderate |
| 5 | Bloom filters passed to a star join's fact table | 13 | small | medium | medium | large | small | none | medium | low |
| 6 | no unindexed nested loop beneath an unbounded outer | 13 | none | none | small | none | medium | none | small | low |
| 7 | the cost model against measured time | 12, 15 | unknown | unknown | medium | small | small | medium | medium | low |
| 8 | bushy plans over connected pairs, with CD-C | 1, 2, 4 | medium | small | medium | small | small | none | large | high |
| 9 | statistics kept as rows change | 5, 11 | none | none | none | none | medium | none | medium | high |
| 10 | general unnesting by dependent joins | 10 | none | small | medium | none | small | none | very large | high |
| 11 | degree-sequence upper bounds as a guard | 14 | small | small | small | small | small | none | medium | low |
| 12 | custom plans, then a generic one | 11 | none | small | small | none | medium | none | medium | moderate |
| 13 | an adaptive join switch and mid-query re-optimization | 13 | medium | small | medium | small | medium | none | very large | high |

The JOB and TPC-H columns rest on step 3's records, the other four on the sources and the queries'
text; each change's subsection says why.

Four of the step's candidates are split where their parts differ in gain, work or risk. The
statistics kept by ANALYZE (rank 2) touch no write; kept as rows change (rank 9) they add work to
every commit. The join search over connected subsets (rank 3) keeps plans left-deep and needs no
change to execution; bushy plans and CD-C (rank 8) do. Robust execution is three changes of very
different size: the nested-loop rule (rank 6), Bloom filters (rank 5), and adaptive joins with
re-optimization (rank 13). Plan caching's re-planning on ANALYZE goes with rank 2, whose statistics
it carries to statements already prepared; its custom plans need ranks 2 and 4's estimates of a
value (rank 12).

### Rank 1. Join sizes from equivalence classes, with key caps

- **What changes.** `optimizer/cardinality.rs` (`JoinedSetRows`, decision 13's one estimate per
  joined set). The `=` terms between two tables' columns put those columns in equivalence classes, a
  union-find over the query's column references built once. A set's rows become its tables' rows
  after their filters, divided for each class by the class's domain once for each table of the set
  it joins past the first, however many terms state it; today every join term multiplies in its own
  selectivity. A class's domain is its largest distinct count: a unique key's rows (the rowid, a
  primary key of one column, a one-column unique index), else the distinct values `sqlite_stat1`
  gives where an index leads the column, else rank 2's sketch; with none, the rows of the class's
  smallest table, its column taken for a key (Ebergen's rule). A join that covers a unique key never
  exceeds its other side's rows (the key cap). Join terms no longer take `sel_eq_unindexed`'s 0.1
  (`constraints.rs`). A class that holds two columns of one table makes them equal in every row the
  set keeps, a filter on that table, counted; so a term that merges two classes never lowers a set's
  divisor, and no added term raises an estimate, as CERT requires (f21).
- **From step 3.** JOB's queries state each class whole: 26c equates five movie_id columns by ten
  terms where four connect them, and every one of the 113 has terms beyond those its classes need
  (`cited.py --job`). Each such term divides again. 17a's estimate falls from 456,178 rows to 1.57
  at mc, which joins title's key and has no filter, while its counted rows grow from 41,840 to
  148,552: at mc two terms complete, t.id = mc.movie_id and the surplus mc.movie_id = mk.movie_id.
  Down the longer plans the estimates fall below a row: 343 of JOB's 977 prefixes, in 88 queries,
  are estimated below one row while counting 1,000 or more.
- **Findings.** f39 (A): Ebergen's equivalence sets sharing one domain, which DuckDB's estimator
  follows, with JOB's intermediate tuples down 66% and 95% and its run time 18% and 25%; f15 (A):
  DuckDB divides by the classes' domains; f16 (A): a set's estimate is its own, as decision 13 has
  it; f43 (A): SQL Server's containment assumption; f11 (A): estimates are what make plans bad.
  Nothing in the research is against it.
- **Audit rows.** 8; 14 for joins on a key (the key cap); and what row 6 left, a join term's
  selectivity multiplied in once a term.
- **Expected effect.** JOB: the fall below a row ends, carried by the 52 queries above. TPC-H:
  little; each of its join terms connects a class, and its error stays near (median 18.8 at SF 1,
  9.39 at SF 10). TPC-DS: large: turso's schema declares no key and no index, so each of its joins
  takes 0.1 today (row 8), and its dimensions' rows become its star joins' domains. SSB: the same
  where its keys go undeclared, none where they are declared. CH-benCHmark: key caps for its joins
  on composite keys (warehouse, district, customer). ClickBench: none, one table.
- **Measurement.** JOB and TPC-H at SF 1 and 10, on the change and its parent. JOB's prefixes
  estimated below one row while counting 1,000 or more: 343 at step 3, target under 35. Its median
  q-error at three joins: 1.07e+03, target under 100; its largest intermediate results' p95:
  7.797e+06, target under 1e+05. JOB's run time over the parent's, geometric mean over the 113:
  target 0.85 or less (the same estimator took 18% off JOB in DuckDB, f39). TPC-H at SF 10: median
  q-error 9.39, not above the parent's.
- **Falsified by.** The 343 not halving; JOB no faster where its estimates come near the counts (the
  geometric mean over the parent at 1 or more); a failure the change brings to CERT or the order
  check, which run nightly.
- **Work and risk.** Small: one file, over statistics ANALYZE keeps today. Moderate risk: every join
  estimate moves, and kiln's statements that join must keep their plans or be measured.
- **Rules.** §11: the union-find and a set's divisors are loops over arrays, bounded by the query's
  terms and the set's tables. §2: the classes bounded by the terms, the memo's 8,192 sets as now.
  §1: checked arithmetic, a domain of at least one, typed errors. §5: decision 14 in
  `docs/design/planner.md`, with Ebergen's thesis and DuckDB's estimator for sources and this
  falsifier. §6: tests in `crates/engine/tests/planner.rs` citing AC-T1.1.2, as decision 13's do.

### Rank 2. Distinct counts and a sample per table, kept by ANALYZE

- **What changes.** ANALYZE (`translate/analyze.rs`, which writes `sqlite_stat1`'s rows per index
  prefix) also keeps a HyperLogLog sketch of each column's values and a reservoir sample of each
  table's rows, its size bounded and measured on the bench, in an internal table beside
  `sqlite_stat1` (the engine's reserved prefix, `schema.rs`), loaded with it into `AnalyzeStats`
  (`stats.rs`). Estimates read them (`constraints.rs`, `estimate_selectivity` and
  `estimate_in_selectivity`): an equality, and each value of an IN list, takes the value's share of
  the sample where the sample holds it often enough to count, else one over the column's distinct
  count, where today an equality on a column no index leads takes 0.1 and an IN list one row a
  value, as if each were unique. A GROUP BY's groups come from its columns' distinct counts capped
  by its input (`estimate_select_output_rows`, which has them today only where an index leads them),
  and rank 1 takes its domains from them where no index leads a join column. A statement prepared
  before an ANALYZE that changed its tables' statistics plans again at its next run, as after a
  schema change (`statement.rs`, `reprepare`). The sample is drawn by a seeded generator and the
  sketches hash by a fixed function, so two passes of one build record the same estimates (step 3's
  repeat test) on every platform.
- **From step 3.** 16 of JOB's 113 first tables are off by 13,417 or more, 15 of them an equality on
  keyword.keyword, which no index covers: 13,417 estimated, a tenth of the table, against 1 counted.
  At SF 10, q16's part, whose IN list counts each of its eight sizes as one row, is estimated at
  1.44 rows against 296,824 (q16's largest q-error 1.187e+06), and q12's lineitem, two shipping
  modes, at 2e-05 against 310,803 (3.108e+05).
- **Findings.** f14 (A): Umbra keeps HyperLogLog sketches on every column (about 2% in 1.5 KB) and a
  sample of every relation, and gets near exact distinct counts and multi-column estimates from
  them; f15 (A): DuckDB's HyperLogLog over sampled vectors; f42 (A): ClickHouse's per-column
  statistics, built by default, HyperLogLog among them; f2 (A), f3 (A): Amphora's `sqlite_stat1` and
  fixed selectivities; f8 (B), f35 (A): Amphora plans again only after a schema change, where the
  engines' plan caches plan again for more. Against: f13 (D), Han et al.'s finding that sampling
  over joins did worse than PostgreSQL's estimator; this sample is one table's, for its own filters,
  which that finding didn't test.
- **Audit rows.** 5; 9, where no index leads a GROUP BY's columns; 11, the half that is new
  statistics.
- **Expected effect.** JOB: the 15 keyword first tables exact, keyword's values being distinct; a
  frequent value at its share of the sample, as cn.country_code = '[us]' at 9a's first table (23,500
  estimated, 84,843 counted), which one over the column's distinct count would put further off.
  TPC-H: q16's and q12's IN lists, and q19's part in part (an OR of three conjunctions, whole in
  rank 4). TPC-DS, SSB and CH-benCHmark: their dimension filters (a year, a region, a category) and
  rank 1's domains. ClickBench: the groups of its 29 GROUP BY queries, which decide hashing or
  sorting (decision 7), and its equalities.
- **Measurement.** JOB's q-error at its first tables: p90 1.342e+04 at step 3, target 11 or less,
  what its other first tables give (10.5 with the 15 keyword prefixes exact), and the 15 within 1.1
  of their counts. TPC-H at SF 10: q16's part within 10 of its count and q12's lineitem within 100
  (their ranges and column comparisons keep fixed selectivities until rank 4). JOB and TPC-H no
  slower than the parent beyond its passes' spread. The audit's `08-fixed-at-prepare`: the join
  prepared before ANALYZE plans again after it.
- **Falsified by.** A keyword prefix off by more than 1.1 after ANALYZE; q16's part off by more than
  10; a plan the new estimates move that runs slower than the parent's.
- **Work and risk.** Large: the statistics table, ANALYZE's sketches and sample in its bytecode, the
  estimates that read them, the re-plan. Moderate risk: every equality and IN estimate moves, so the
  bench suite's 52 statements and kiln's must keep their plans or be measured, and ANALYZE takes
  longer (measured beside `sqlite_stat1`'s cost).
- **Rules.** §2: a sketch's 1.5 KB a column and the sample's rows charged to a statistics budget; a
  table past it keeps `sqlite_stat1` alone and says so. §5: the sample's size measured, the sketch's
  precision cited, a decision record. §12: the hash and the generator the same on every platform.
  §11: ANALYZE's loops in bytecode. §1. The previous release and SQLite open the file as before: the
  new table is an internal one neither reads.

### Rank 3. Connected subsets within a counted budget, the greedy search fixed

- **What changes.** `optimizer/join.rs`. The dynamic program walks connected subsets only, as DPccp
  enumerates them (f22), on an explicit stack, each extended by one table so plans stay left-deep,
  and counts them against a budget measured on the bench (Neumann and Radke solve exactly up to
  10,000 connected subgraphs, f23) in place of `GREEDY_JOIN_THRESHOLD`'s 12 tables; the walk of
  every subset (`generate_join_bitmasks`, then `has_connected_legal_candidate`) goes. Past the
  budget the greedy search adds at each step the table whose joined set has the fewest estimated
  rows, one lookup in `JoinedSetRows`, where it now compares the plan's cost so far and counts a
  table's filter for nothing at the step it adds it (row 2). It is no longer called GOO, which
  merges pairs into bushy trees (f28) and comes with rank 8. Its `unwrap` and two `expect`s
  (`compute_greedy_join_order`, `find_best_starting_table`) become typed errors.
- **From step 3.** Amphora is the faster on JOB's queries of 10 to 12 tables (SQLite 1.813) and the
  slower on the 9 above 12 (0.405): 33c takes 692 ms where SQLite takes 30.9, 29c 3.46e+03 where
  SQLite takes 570. Row 2 shows the greedy's plan at 5.7 times the dynamic program's cost by
  Amphora's own model. 28a to 28c have 2,032 connected subsets and 33a to 33c 1,517, inside a budget
  of 10,000; 29a to 29c have 13,246 (`cited.py --job`).
- **Findings.** f22 (A): DPccp meets the lower bound, while DPsize, which the search resembles, runs
  far above it; f23 (A): exact within a budget of connected subgraphs, not a table count; f27 (A):
  on JOB exhaustive search beat the heuristics even with estimation errors (GOO up to 21 times
  costlier); f28 (A): GOO as published; f1 (A): the search today; f10 (A), f24 (A): DPhyp and
  DuckDB's enumerator are recursive as published, which Amphora's rules forbid.
- **Audit rows.** 2, 3, and row 16's three calls in the greedy search.
- **Expected effect.** JOB: its 9 queries above 12 tables planned exactly or by a greedy that counts
  filters; planning time where the dynamic program walks fewer subsets (27a to 27c: 905 connected of
  the 4,095 subsets of 12 tables). TPC-DS: Q64's 18 tables. TPC-H, SSB and CH-benCHmark (9 tables at
  most) and ClickBench: none but planning time.
- **Measurement.** JOB: SQLite's time over Amphora's on the 9 above 12 tables, 0.405 at step 3,
  target 1 or more. Planning time: at most 92.1 ms for every JOB query, step 3's most (27c, 12
  tables), and a median not above step 3's 1.9 ms.
- **Falsified by.** The 9 no faster than on the parent; a query past the budget slower than its
  parent's plan; an order the walk of every subset finds and the connected walk misses (a test over
  random join graphs runs both).
- **Work and risk.** Medium: one file, the search's frame kept. Moderate risk: every join order
  passes through it, which the order check and the planner tests watch.
- **Rules.** §11: the explicit stack. §2: the budget, counted. §1: the three calls typed errors,
  `scripts/engine-ratchet.toml` lowered with them. §5: the budget measured on the running hardware,
  planning time against plan cost. §6.

### Rank 4. Each table's filters estimated together on its sample

- **What changes.** `optimizer/constraints.rs` and `cost.rs` (`constraints_selectivity`). Once per
  table per prepare the planner runs the table's filter terms over its sample (rank 2's) as one
  program bounded by the sample's rows, and takes the share that passes as their selectivity:
  ranges, LIKE, functions and correlated columns at once (Oracle samples a table's blocks at parse
  time for the same, f13). A conjunction no sample row passes takes half a row of the sample, never
  above the same conjunction with a term fewer, whose rows include its rows (CERT, f21). A term that
  raises an error on a sample row gives no share. Where no sample serves (a bound parameter's value,
  a table without one, such a term), a table's terms combine by exponential backoff, the most
  selective first with exponents 1, 1/2, 1/4 and 1/8 (SQL Server's CE 120, f17), in place of their
  product; with the terms ordered so, an added term multiplies the result by at most 1, and CERT
  holds there too. Equalities on an index's leading columns take the index's distinct prefixes from
  `sqlite_stat1` as a multi-column distinct count, what PostgreSQL's extended statistics are made by
  hand for (f17).
- **From step 3.** At SF 10 q19's part, an OR of three conjunctions, is estimated at 1.2 rows and
  counts 239,849 (q19's largest q-error 1.999e+05), and each of those rows searches lineitem:
  1.011e+04 ms where SQLite takes 612 and PostgreSQL 448. q06's lineitem, three ranges, is off by
  1.9e+03. With its 15 keyword prefixes taken as exact, JOB's first tables' q-errors have a p95 of
  66.3, what the rest of their filters leave.
- **Findings.** f17 (A): correlated columns break the independence assumption, with PostgreSQL's
  extended statistics and SQL Server's exponential backoff the engines' answers; f13 (D): samples
  catch correlations per-column statistics miss, while single-table samples miss join-crossing ones,
  graded D for Han et al.'s counterclaim on sampling over joins, which this is not; f14 (A): Umbra's
  sample gives multi-column estimates; f21 (A): CERT.
- **Audit rows.** 5, the independence assumption.
- **Expected effect.** JOB: its LIKE and range filters (on title, name, char_name) and the
  correlations within a table; join-crossing correlations stay (f13). TPC-H: q19's plan; q12's
  ranges and column comparisons; q06's estimate, though q06 stays a scan (no index leads
  l_shipdate). SSB: Q1.1 to Q1.3's three ranges on lineorder and the date. ClickBench: the filters
  most of its 43 queries have. TPC-DS: its dimensions' ranges. CH-benCHmark: its date ranges.
- **Measurement.** TPC-H at SF 10: q19's part, 1.2 rows estimated against 239,849 at step 3, target
  within 10 of its count, and q19's time, 1.011e+04 ms, target under twice SQLite's 612; q06's
  lineitem, off by 1.9e+03, target within 2. JOB's first tables' p95, 1.342e+04 at step 3, target
  under 20. Planning time: JOB's median, 1.9 ms at step 3, target at most twice that.
- **Falsified by.** q06's estimate off by more than 2 at the sample's size; q19 no faster once its
  part's estimate is near its count; planning time past the target; a CERT failure.
- **Work and risk.** Medium once rank 2 is in: an evaluator over the sample at prepare, and backoff
  in `constraints_selectivity`. Moderate risk: every prepare runs a program a table.
- **Rules.** §2: the sample's run bounded by its rows and a counted budget of steps. §11. §1 and §3:
  an error on a sample row ends that estimate, never the statement. §5: the exponents SQL Server's,
  cited; the half row stated. §6: CERT's and the order check's shapes as tests.

### Rank 5. Bloom filters passed to the fact table of a star join

- **What changes.** `optimizer/join.rs` and `translate/main_loop` (`hash.rs`, `seek.rs`), with the
  Bloom filters the VDBE has (`vdbe/bloom_filter.rs`, and the `Filter` instruction a hash join's
  probe tests today, for its own build only). Where a plan reads a large table and then searches
  filtered tables by key (a star), each such table's filtered keys go into a Bloom filter built
  before the large table's loop, and each of its rows tests them all before any search (Lookahead
  Information Passing, f33); predicate transfer passes them along the join graph for other shapes
  (f33). A filter only drops rows no join would keep; its false positives go on to the search.
- **From step 3.** At SF 10 q09 reads lineitem first and searches part for each of its 59,986,052
  rows, of which 3,261,613 pass part's filter: 1.585e+05 ms where PostgreSQL takes 2.380e+04. q05
  takes 1.634e+04 against 8.15e+03, q08 3.112e+04 against 2.19e+03.
- **Findings.** f33 (A): LIP, predicate transfer (3.3 times over Bloom join on TPC-H, its join phase
  robust to the order), POLAR and diamond-hardened joins; f32 (A).
- **Audit rows.** 13, the Bloom filters.
- **Expected effect.** SSB: every query a star of lineorder and one to four dimensions, LIP's shape.
  TPC-H: q05, q08 and q09. TPC-DS: its store_sales and catalog_sales stars. JOB: little, its plans
  starting at a filtered table already. CH-benCHmark: its order-line joins. ClickBench: none.
- **Measurement.** TPC-H at SF 10: q09, 1.585e+05 ms at step 3, target twice as fast as its parent;
  q05 and q08 faster than their parents. SSB, loaded and measured on the parent first.
- **Falsified by.** q09 no faster though its filters drop the rows part's filter would (the row
  counters show it); a query slower by more than its filters' building.
- **Work and risk.** Medium: the filters and their instruction exist; building them ahead of the
  large table's loop is new. Low risk: a filter never changes a result.
- **Rules.** §2: each filter's bits charged to the statement's memory, sized from its keys'
  estimate; one past its budget isn't built. §5: the false-positive rate
  (`DEFAULT_FALSE_POSITIVE_RATE`, 1% today) measured or cited. §11. §6.

### Rank 6. No nested loop without an index beneath an outer not bounded at one row

- **What changes.** `optimizer/join.rs` and `access_method.rs`. A loop that scans a table whole for
  each row before it is planned only when the rows before it are bounded at one: equalities on a
  unique key (the rowid, a primary key, a unique index) or a LIMIT 1. Otherwise the two sides join
  by hashing, the smaller side built, or through an automatic index, so the table is read once
  whatever the outer counts (Leis et al. took such loops away and JOB's timeouts ended, f32).
- **From step 3.** 30 of JOB's 113 plans hold such a loop, each beneath a first table estimated at
  0.4 or 1.2 rows by a fixed selectivity. Each counted 1 row, so each scan ran once: on step 3's
  data they cost nothing. TPC-H's plans hold none (0 at SF 1 and 0 at SF 10). The audit's row 13
  shows the case that goes wrong: p and q analyzed at 10 rows and grown to 3,000, q scanned for
  every row of p.
- **Findings.** f32 (A); f11 (A).
- **Audit rows.** 13, nested loops on uncertain estimates.
- **Expected effect.** JOB and TPC-H: none measured, the 30 scans having run once (a one-row build
  reads the same rows). CH-benCHmark: its tables grow past their statistics while it runs (rows 11
  and 13). TPC-DS: joins with no index, where a misestimated outer would scan again and again. SSB
  and ClickBench: none.
- **Measurement.** JOB: plans with such a loop, 30 at step 3, target 0, their times within the
  passes' spread of their parents'. TPC-H: none at step 3, none after. The audit's `10-no-adaptive`
  at 3,000 rows a table: q read once.
- **Falsified by.** A plan the rule changes that runs slower than its parent's on JOB, TPC-H or the
  bench suite.
- **Work and risk.** Small: one condition in the search. Low risk.
- **Rules.** §5: a decision record citing f32. §6. §1.

### Rank 7. The cost model against measured time

- **What changes.** `optimizer/cost.rs` and `cost_params.rs`. A table's and an index's pages come
  from the B-tree's page count, which ANALYZE records, where today they are rows over 50
  (`rows_per_table_page`, row 12). The cost units (a page read, a seek, a row, a hash probe, a
  sort's row) are fitted to times measured on the running hardware, where the inherited ones
  (`cpu_cost_per_row`, `cpu_cost_per_seek`, the hash and sort costs) are neither measured nor cited,
  as CLAUDE.md §5 requires a tuning constant to be. The planner bench adds P-error (f20): each
  plan's cost under counted rows against the cost of the plan chosen with counted rows in place of
  estimates, those rows given through `JoinedSetRows` for every set the search asks for, each set
  counted once and kept beside the records.
- **From step 3.** The cost each JOB plan is estimated at ranks the queries' measured times at a
  Spearman correlation of 0.20, estimates and costs together; P-error is not measured (row 15).
- **Findings.** f12 (D): on JOB cost-model errors were dwarfed by estimation errors, graded D for
  Amphora's own counterclaim (decisions 2, 5 and 7); f20 (A): P-error tracks end-to-end time; f29
  (A): Calcite's cost combines CPU, I/O and memory; f9 (A): Amphora's cost model.
- **Audit rows.** 12, 15.
- **Expected effect.** ClickBench (hits' wide rows), TPC-DS's fact tables and JOB's text tables
  (movie_info, person_info): scans costed at their pages, no longer cheap against seeks. TPC-H, SSB
  and CH-benCHmark: where costs decide between plans near in rows. Beyond that, P-error says which
  plans ranks 1 to 4 moved.
- **Measurement.** JOB and TPC-H: the rank correlation of estimated cost with time, 0.20 on JOB at
  step 3, target 0.8 or more with counted rows in place of estimates; P-error for every query, none
  at step 3. The audit's `09-cost-model`: its tables of 8 and 1,011 pages no longer both scan at a
  cost of 46.
- **Falsified by.** The correlation with counted rows no higher than with estimates.
- **Work and risk.** Medium: page counts in ANALYZE, a fitting run, the P-error harness. Low to
  moderate risk: each cost change moves plans, which the bench suite measures.
- **Rules.** §5: each constant measured on the running hardware, its comment saying so. §7: the
  fitting run recorded in `bench/`. §6.

### Rank 8. Bushy plans over connected pairs, with the CD-C conflict detector

- **What changes.** `optimizer/join.rs` and the emitter. The search enumerates connected complement
  pairs (DPccp, or DPhyp for terms over more than two tables, f22) on rank 3's explicit stack and
  budget, so a join can take two composite inputs; past the budget, linearized dynamic programming
  or GOO as published, merging the pair of least cost (f23, f28). A join's result becomes a hash
  join's build through the emitter's materialized build input, which already folds a join prefix
  into a build (`MaterializedBuildInput` in `emitter/mod.rs`, made in `emitter/select.rs`). Outer,
  semi and anti joins reorder by Moerkotte, Fender and Eich's CD-C conflict detector (f25) in place
  of `required_lhs_by_table`'s barriers (row 4).
- **From step 3.** PostgreSQL, which searches bushy trees (f26), is the faster on 83 of JOB's 113
  (its time over Amphora's 0.354), by about as much where Amphora's estimates are near as where they
  are off (0.362 and 0.344): its lead rests on something else, and its plans' shape is one
  difference.
- **Findings.** f27 (A): left-deep trees up to 4.5 times the best bushy plan on JOB, zig-zag 2.54;
  f22 (A), f24 (A), f26 (A): DPhyp in DuckDB, bushy search in PostgreSQL; f25 (A): CD-C, correct and
  complete, in ClickHouse and CockroachDB; f23 (A), f28 (A); f10 (A): DPhyp is recursive as
  published.
- **Audit rows.** 1, 4, and row 2's GOO.
- **Expected effect.** JOB: the queries whose best plan joins two composite inputs. TPC-DS: its
  queries over two fact tables (Q17, Q25, Q29) and Q64, and its LEFT JOINs (Q51's and Q97's FULL
  JOINs need no reordering, row 4). TPC-H: little (8 tables at most), its unnested semi and anti
  joins (Q4, Q22) free to move. SSB: little, stars of 5 tables. CH-benCHmark: Q13's LEFT JOIN and
  its unnested EXISTS. ClickBench: none.
- **Measurement.** JOB: PostgreSQL's time over Amphora's, 0.354 at step 3; target JOB's time over
  its parent's at most 0.95 by geometric mean, and each query whose plan turns bushy faster than on
  its parent. The audit's `03-right-full-barrier`: the RIGHT JOIN reads tiny first, total_cost
  23,007.0 where it is 328,300,039,000.3.
- **Falsified by.** No JOB or TPC-H plan turning bushy; those that do no faster; a wrong result in
  the oracles (DQP compares one query's plans).
- **Work and risk.** Large: the search, a composite build in the emitter, CD-C's rules. High risk:
  new shapes of execution and outer joins reordered, which DQP, TLP and NoREC test.
- **Rules.** §11: DPhyp's recursive enumeration as loops over an explicit stack. §2: rank 3's
  budget, and a composite build charged to the hash table's budget. §1. §5. §6.

### Rank 9. Statistics kept as rows change, and statements re-planned as they move

- **What changes.** The write path (the inserts and deletes of `vdbe/execute.rs`, applied at
  commit): a commit's inserted values join their columns' sketches, whose merge is a register-wise
  maximum, so writers' additions merge in any order; its rows join the table's sample by online
  reservoir sampling, a deleted sampled row replaced by an upcoming one (Birler, Radke and Neumann,
  f41); the table's row count follows each commit. A table never analyzed has statistics from its
  first rows (ClickHouse builds its statistics on insert, f42). Deletes leave the sketches
  over-counting until the next ANALYZE; the rows changed since are counted with the statistics, and
  a statement plans again when its tables' rows have moved by a stated share since it was planned.
- **From step 3.** No step-3 number applies: each benchmark was analyzed after loading and each
  query prepared after (section 8). A re-plan costs a prepare: a median of 1.9 ms on JOB and 0.5 to
  0.6 ms on TPC-H at step 3.
- **Findings.** f41 (A), f14 (A), f42 (A); f8 (B), f35 (A).
- **Audit rows.** 5, the fallbacks without ANALYZE; 11, for tables that grow.
- **Expected effect.** CH-benCHmark: its analytical queries over tables its transactions grow. The
  other five load, analyze and run: none. kiln prepares its statements once a connection while its
  tables grow, so for kiln this change matters more than its rank says.
- **Measurement.** CH-benCHmark, loaded and run on the parent first (step 3 has none): its
  analytical queries' times as its tables grow, target faster than the parent's with ANALYZE run on
  the same schedule, each re-plan costing a prepare (a median 1.9 ms on JOB at step 3). The bench
  suite's writes: target within 2% of the parent, with the write rate and p99 of AC-T1.1.3 and
  AC-T1.1.4 held at K0's load.
- **Falsified by.** A write statement slower beyond the 2%; CH-benCHmark's queries no faster than on
  the parent with ANALYZE run on the same schedule.
- **Work and risk.** Medium. High risk: it adds work to every commit, the path T1's acceptance
  criteria measure.
- **Rules.** §2: a commit's added work bounded. §8: deterministic in the log's order, so every
  replica keeps the same statistics. §10: its metrics agreed with kiln's Observe owner. §5. §6:
  deterministic simulation with writers and a nemesis.

### Rank 10. General unnesting by dependent joins, top-down, on a worklist

- **What changes.** `optimizer/unnest.rs`, which unnests by fixed patterns, gives way to Neumann and
  Kemper's dependent joins, pushed down until no correlation is left (f30), in Neumann's top-down
  form that visits each operator once (f38), each step taken from an explicit worklist where both
  papers recurse (f10). NOT IN, subqueries beside a RIGHT or FULL JOIN and those that read several
  tables unnest too; the comparison of the two forms' costs (`mod.rs`) stays.
- **From step 3.** Of step 3's queries only TPC-H q21 keeps its subqueries (EXISTS and NOT EXISTS
  over four outer tables, on cost): 5.309e+04 ms at SF 10 where PostgreSQL takes 1.195e+04 and
  DuckDB 3.54e+03. Its plan's order counts too: it reads lineitem first and nation last.
- **Findings.** f30 (B); f31 (A): DuckDB and CedarDB decorrelate every subquery; f38 (C): the 2025
  top-down form, which made PostgreSQL's 26 minutes on its Figure 1 query 1.3 s; f7 (A): the
  patterns today; f10 (A).
- **Audit rows.** 10.
- **Expected effect.** TPC-DS: ten of its queries keep correlated subqueries (Q1, Q6, Q10, Q16, Q30,
  Q35, Q41, Q69, Q81, Q94, row 10). CH-benCHmark: Q16's NOT IN and its EXISTS forms. TPC-H: q21
  where its unnested form is the cheaper. JOB, SSB and ClickBench: none, having no subqueries.
- **Measurement.** TPC-H at SF 10: q21, 5.309e+04 ms at step 3, target under PostgreSQL's 1.195e+04
  where it is unnested. TPC-DS, loaded and its ten queries measured on the parent first.
- **Falsified by.** TPC-DS's ten queries no faster unnested; a finding of the oracles (TLP, NoREC,
  DQP, the differential check) on an unnested form.
- **Work and risk.** Very large: dependent joins in a plan that holds joined tables, terms and
  subqueries, with no operator tree. High risk: NOT IN's and outer joins' NULLs, which the oracles
  test.
- **Rules.** §11: the worklist. §2: a counted bound on rewrites. §3: a form that can't be unnested
  stays correlated, and EXPLAIN says so. §1. §5. §6.

### Rank 11. Degree-sequence upper bounds as a guard

- **What changes.** ANALYZE's ordered scan of each index, which counts its distinct prefixes for
  `sqlite_stat1`, also keeps the most rows one value holds: the largest degree, the ℓ∞ norm of its
  degree sequence (LpBound bounds joins from such norms, f19). A joined set's upper bound is one
  table's rows times, along a spanning tree of its joins, each join's largest degree; an estimate
  above its bound is cut to it, and a decision an estimate carries (rank 6's) can see how far above
  it the bound lies.
- **From step 3.** Step 3's errors are mostly underestimates, 791 of JOB's 977 prefixes, which an
  upper bound leaves as they are; LpBound uses its bounds as the estimates, and SafeBound's authors
  call theirs not ready for production (f40).
- **Findings.** f19 (A), f40 (C), f11 (A).
- **Audit rows.** 14.
- **Expected effect.** Small on JOB, TPC-H, TPC-DS, SSB and CH-benCHmark: an estimate above a bound
  is rare where statistics exist, and the bound caps the cross joins of tables without them (f6).
  ClickBench: none, one table.
- **Measurement.** JOB and TPC-H: prefixes estimated above their bound, target 0; step 3's 791
  underestimates left as they are.
- **Falsified by.** A prefix counted above its bound (a property test over random data).
- **Work and risk.** Medium: the degrees in ANALYZE, a bound per set. Low risk.
- **Rules.** §2, §5, §11.

### Rank 12. Custom plans, then a generic one, for prepared statements

- **What changes.** `statement.rs`: a statement with parameters is planned with its bound values for
  its first five runs, then generically if the generic plan's estimated cost is not significantly
  above those custom plans' average (PostgreSQL and CockroachDB, f35); ranks 2 and 4 give a value
  its estimate. A statement keeps two plans at most, the generic one and its current run's.
- **From step 3.** No step-3 number applies to the plans: JOB and TPC-H ran literal queries. The
  audit's `08-fixed-at-prepare`: `WHERE k = ?` has one plan and one estimate, 1,000 rows, where k =
  1 counts 1,000 and k = 9 none.
- **Findings.** f35 (A): custom then generic plans in PostgreSQL and CockroachDB, several cached
  variants by the parameters' cardinality in SQL Server (PSP), a recompile when a bound value could
  change the plan in SQLite; the first keeps the fewest plans, two, which §2 favours. f8 (B):
  Amphora plans once.
- **Audit rows.** 11, a parameter's value.
- **Expected effect.** TPC-H and TPC-DS run as prepared streams with substitution parameters, and
  CH-benCHmark's transactions; JOB, SSB and ClickBench run literal queries: none.
- **Measurement.** TPC-H's 22 prepared once and run with qgen's parameters, on the parent first
  (step 3 has none), target no query slower than its generic plan; the planning paid on each custom
  run, 0.5 to 0.6 ms a TPC-H query at step 3, target the bench suite's prepared statements within 2%
  of the parent.
- **Falsified by.** Custom plans no faster than the generic one; the bench suite's prepared
  statements slower beyond 2% for their extra planning.
- **Work and risk.** Medium. Moderate risk: five more plannings of every statement with parameters.
- **Rules.** §2: the two plans. §5: the five runs PostgreSQL's, cited, or measured. §6.

### Rank 13. An adaptive join switch and mid-query re-optimization

- **What changes.** The emitter and the VDBE: a join emitted as a hash join and as an index nested
  loop, chosen when its outer side's rows are counted against a threshold (SQL Server's batch-mode
  adaptive join, f32); and where a plan materializes rows (a hash build, a sort), the rows counted
  compared with the estimate and the rest of the plan made again when they differ by a stated factor
  (Perron et al., f34). Decision 12's row counters are the counting points.
- **From step 3.** JOB's 20 slowest queries take 414,668 ms of Amphora's 514,502 (PostgreSQL 103,893
  ms on the same 20); Perron et al. took 27% off JOB's 20 longest-running (f34).
- **Findings.** f32 (A); f33 (A): POLAR; f34 (A): re-optimization, and holistic adaptivity absent
  from commercial systems because it complicates debugging and testing.
- **Audit rows.** 13, adaptive execution.
- **Expected effect.** JOB: its long tail. TPC-H, TPC-DS, SSB and CH-benCHmark: where estimates stay
  off after ranks 1 to 4. ClickBench: none.
- **Measurement.** JOB: the 20 slowest queries' time against the parent's, 414,668 ms of 514,502 at
  step 3, target 27% less (f34).
- **Falsified by.** Under 27% on JOB's 20 slowest once ranks 1 to 4 are in (they take the same
  misestimates away); a result changed (the oracles).
- **Work and risk.** Very large: a program compiled whole would need a second plan in mid-run. High
  risk.
- **Rules.** §2: what is materialized charged to a budget. §3. §11. §6: the oracles over both paths.

### Not ranked: learned estimators

f18 (D): learned estimators are more accurate than traditional ones on static data, costly to train
and to run, unable to keep up with fast updates, and unpredictable; against it, Han et al. found ML
data-driven estimators improving PostgreSQL's query times markedly where most traditional methods
hardly did. Amphora's records change as kiln writes them, a model's training and memory need bounds
(§2) its sources don't give, and nothing in them keeps CERT's property, which steps 4 and 5 made
hold. They stay unranked until ranks 1 to 4 are in; a measurement on Amphora's workloads would
reopen them if JOB's largest intermediate results' p95 then stays above 1e+03 (7.797e+06 at step 3).

### Why this order

Each reason names the gain on the six benchmarks, the work and the risk, so that one can be disputed
alone.

- **1 before 2.** Both are estimation, where step 3 puts the time (the 52 queries). Rank 1's cause
  is in the records and in JOB's text (343 prefixes; surplus terms in all 113), and its fix is one
  file over statistics ANALYZE keeps today; rank 2 needs new statistics, a table and a sample, for
  errors that sit mostly at first tables (16 of JOB's 113) and in TPC-H's IN lists.
- **2 before 3.** Rank 2's estimates reach all six benchmarks; rank 3 reaches JOB's 9 queries above
  12 tables and TPC-DS Q64, with a gain step 3 measured (0.405 against 1.813) and less work. Whoever
  weighs that measured gain above the breadth puts rank 3 second.
- **3 before 4.** Rank 3's gain is measured and its work medium; rank 4 waits for rank 2's sample
  and needs an evaluator at prepare, and of its step-3 targets q19 moves a plan while q06 can't (a
  scan whatever its estimate).
- **4 before 5.** Rank 4 is estimation on every benchmark; rank 5's measured target is TPC-H's three
  star queries until SSB is loaded, though its risk is lower.
- **5 before 6.** Rank 5 has a gain to measure (q09's 1.585e+05 ms); rank 6 changes no plan step 3
  timed (its 30 loops each ran once), for less work. It is insurance: whoever wants the insurance
  first swaps them.
- **6 before 7.** Rank 6 is one condition at low risk; rank 7's gain on JOB and TPC-H is unknown
  until P-error is measured, and its work is a fitting run and a harness.
- **7 before 8.** Rank 7 says which plans the estimates moved, at low risk; rank 8 is large work at
  high risk, for a gain the sources bound (left-deep within 4.5 times of bushy on JOB, f27) and step
  3 can't place.
- **8 before 9.** Rank 8 reaches JOB and TPC-DS; rank 9 reaches only CH-benCHmark of the six and
  puts work on every commit. For kiln's own statements rank 9 would come first: this order is the
  benchmarks'.
- **9 before 10.** Both are high risk; rank 9 is medium work for one benchmark, rank 10 very large
  work for TPC-DS's ten queries and CH-benCHmark's, none measured yet.
- **10 before 11.** Rank 10 changes plans on TPC-DS and CH-benCHmark; rank 11 caps estimates where
  step 3's errors are mostly underestimates (791 of 977), so it changes little, though at low risk.
- **11 before 12.** Rank 11 is low risk; rank 12's gain is on runs step 3 didn't make (prepared
  streams), and it plans every statement with parameters five more times.
- **12 before 13.** Rank 13's gain (27% on JOB's slowest, f34) is the misestimates ranks 1 to 4 take
  away, at the most work and risk of any.

### The audit's gaps, each with a change or a reason

| audit item | ranked change | reason, where none |
|---|---|---|
| 1. left-deep trees only | 8 | |
| 2. above 12 tables a greedy left-deep search, called GOO | 3, 8 | |
| 3. subsets enumerated by size, not as connected pairs | 3 | |
| 4. no conflict detector | 8 | |
| 5. statistics only `sqlite_stat1`; fallbacks without ANALYZE; filters multiplied | 2, 4, 9 | |
| 6. a join's estimate from the access path chosen (AG) | — | closed since the audit by decision 13 (d762dc2) and step 5 (10f2461): the order check failed none of 502,632 and 530,789 checks, CERT none of 2,607,858; rank 1 changes how a set's rows are computed, not this |
| 7. a FULL JOIN's estimate leaves out the right table's unmatched rows | — | closed by decision 13: a FULL JOIN keeps both sides' unmatched rows (`a_full_join_is_estimated_at_least_its_inner_join`) |
| 8. a join on columns no index leads takes a fixed 0.1 | 1 | |
| 9. a GROUP BY's groups are not estimated | 2 | |
| 10. unnesting by fixed patterns | 10 | |
| 11. plans fixed at prepare | 2, 9, 12 | |
| 12. a page-based cost model whose pages are rows over 50 | 7 | |
| 13. no adaptive or robust execution | 5, 6, 13 | |
| 14. no upper bound on an estimate | 1, 11 | |
| 15. P-error not yet measured | 7 | |
| 16. unwrap, expect and panics in the greedy search and the planner | 3 | the calls elsewhere go with whichever change touches them (§1, `scripts/engine-ratchet.toml`) |
| learned estimators (f18, D) | — | not ranked: the subsection above |
| Calcite's rule engines (f29, A) | — | a framework's design, not a technique Amphora lacks for these benchmarks (section 7) |
| the rules on how a change is built (f10, A) | — | not a gap: they bind every change, each in its rules |

Of the 19 items, 14 have a ranked change and 5 a reason; 0 have neither.

## What would falsify these notes

- A finding's quote missing from its source at the stated location. Kiln re-checks each quote. The
  revisions above fix where the code quotes are.
- For the engines' code: a later revision changing the cited lines (DuckDB's 10,000-pair cutoff or
  12-relation threshold, ClickHouse's `dpsub,greedy` default, its 0.33 and 0.01 factors, or
  CockroachDB's 8-join limit). The table states what these revisions do, not what later ones do.
- For the papers: a replication on JOB or STATS-CEB contradicting a headline result (estimates over
  costs, f12, already has one from Amphora's own workload; learned estimators, f18, have Han et
  al.'s).
- For the audit (section 7): an output of `bench/planner/audit/run.sh --check` differing from the
  one kept, on f734df4; a file and line it cites at f734df4 not showing what its row says; or a
  benchmark query its rows name whose text lacks the shape named (`benchmarks.tsv` is a scan of
  the text, checked by hand where cited).
- For the baseline (section 8): `bench/planner/summary.py` over its records giving other numbers
  than the record's; a pass of `record.sh` on 26657fd recording another estimate at any prefix; or,
  at a load under 2 on jupiter, run times outside the repeat's spread.
