/// Cost model parameters for query optimization.
///
/// These parameters control the heuristics used by the query optimizer for
/// cost estimation. They can be tuned to improve plan selection for specific
/// workloads (e.g., TPC-H).
///
/// # JSON Loading (requires `optimizer_params` feature)
///
/// When the `optimizer_params` feature is enabled, parameters can be loaded
/// from a JSON file via the `TURSO_OPTIMIZER_PARAMS` environment variable.
/// The JSON file does not need to specify all fields, and unspecified fields will use the default values.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct CostModelParams {
    // === Cardinality Fallbacks (when no ANALYZE stats) ===
    /// Assumed rows per table when statistics unavailable.
    pub rows_per_table_fallback: f64,

    /// Rows a page of the table being costed: set for each table by [Self::for_table] from what
    /// ANALYZE counted or the connection's page size (PL.7, T-PL.7.1); until then, the format's
    /// default page of the fewest-byte one-column records (`cost::table_rows_per_page`).
    pub rows_per_table_page: f64,

    /// Amphora (PL.7): the usable bytes of a page, the connection's once planned for it; the
    /// format's default page until then (`cost::FORMAT_DEFAULT_PAGE_BYTES`).
    pub page_bytes: f64,

    /// Amphora (PL.7, T-PL.7.1): the rows a page of each index of the table being costed, as
    /// ANALYZE counted its tree, keyed by normalized name; set by [Self::for_table]. Not a tuning
    /// parameter.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) index_rows_per_page: Option<crate::sync::Arc<rustc_hash::FxHashMap<String, f64>>>,

    // === Selectivity Fallbacks ===
    /// Selectivity for equality predicate on unindexed column (e.g., `x = 5`).
    pub sel_eq_unindexed: f64,

    /// Selectivity for equality predicate on indexed column.
    /// Should be <= sel_eq_unindexed since indexes imply higher selectivity.
    pub sel_eq_indexed: f64,

    /// Selectivity for range predicates (>, >=, <, <=).
    pub sel_range: f64,

    /// Selectivity for a closed range on one column (`x > ? AND x < ?`, `x BETWEEN ? AND ?`), in
    /// place of its two bounds' own: PostgreSQL's `DEFAULT_RANGE_INEQ_SEL`
    /// (src/include/utils/selfuncs.h), what it assumes of a range no statistics place. A seek's
    /// range and a filter's take the same estimate (docs/design/planner.md, decision 1).
    pub sel_closed_range: f64,

    /// Selectivity for IS NULL predicate.
    pub sel_is_null: f64,

    /// Selectivity for IS NOT NULL predicate.
    pub sel_is_not_null: f64,

    /// Selectivity for LIKE predicate.
    pub sel_like: f64,

    /// Selectivity for NOT LIKE predicate.
    pub sel_not_like: f64,

    /// Selectivity for other/unknown predicates.
    pub sel_other: f64,

    /// Estimated rows from IN subquery when actual count unknown.
    /// Matches SQLite's estimate (where.c line 3230).
    pub in_subquery_rows: f64,

    // === Scan/Seek Cost Weights ===
    /// CPU cost per row processed (relative to page IO = 1.0).
    pub cpu_cost_per_row: f64,

    /// CPU cost for one extra operation in a `WHERE` condition.
    pub cpu_cost_per_where_step: f64,

    /// CPU cost per index seek (key comparisons).
    pub cpu_cost_per_seek: f64,

    /// Amphora: pages a read finds without reading the file (`docs/design/planner.md`, decision 5):
    /// the connection's page cache and its database's shared pages. The default is the engine's
    /// default page cache, 2,000 pages; planning replaces it with the connection's.
    pub cache_pages: f64,

    /// Amphora (QT.5): the share of the pages a plan would first read from a cold cache that it
    /// reads from the file: the connection's recent page fetches' share that read a file
    /// (`Pager::page_miss_ratio`), set as the statement is planned. 1, every such page read, until
    /// the connection has fetched a page; pages a cache too small for the plan reads again are read
    /// whatever it is (`cost::file_reads`).
    pub page_miss: f64,

    /// Amphora: the memory a statement's hash and group tables hold together before they spill,
    /// in bytes (`docs/design/executor.md`, decision 8): the connection's own cache
    /// (`Pager::statement_memory_budget`). Planning replaces the default, nothing, with it.
    pub statement_memory: usize,

    // === Sort Cost ===
    /// CPU cost per row for sorting (used in O(n log n) estimate).
    /// This is used when estimating the cost saved by using an ordered index.
    pub sort_cpu_per_row: f64,

    // === Hash Join Cost ===
    /// CPU cost to compute hash of a row.
    pub hash_cpu_cost: f64,

    /// CPU cost to insert row into hash table.
    pub hash_insert_cost: f64,

    /// CPU cost to probe hash table.
    pub hash_lookup_cost: f64,

    /// Estimated bytes per row for hash table spill estimation.
    pub hash_bytes_per_row: f64,

    /// Amphora (PL.12): the values the estimates may read for the statement's parameters, a run's
    /// for a custom plan and none for the generic one, where they note that one asked
    /// (`plan_cache.rs`). Not a tuning parameter: set as the statement is planned.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) plan_values: Option<crate::sync::Arc<crate::plan_cache::PlanValues>>,
}

impl CostModelParams {
    /// Create default parameters as a const fn (for compile-time static).
    pub const fn new() -> Self {
        Self {
            // Cardinality fallbacks
            rows_per_table_fallback: 1_000_000.0,
            rows_per_table_page: super::cost::rows_per_leaf_page(
                super::cost::FORMAT_DEFAULT_PAGE_BYTES,
                super::cost::least_record_bytes(1.0),
                super::cost::varint_bytes(1_000_000.0),
            ),
            page_bytes: super::cost::FORMAT_DEFAULT_PAGE_BYTES,
            index_rows_per_page: None,

            // Selectivity fallbacks
            sel_eq_unindexed: 0.1,
            sel_eq_indexed: 0.001,
            sel_range: 0.4,
            sel_closed_range: 0.005,
            sel_is_null: 0.1,
            sel_is_not_null: 0.9,
            sel_like: 0.2,
            sel_not_like: 0.2,
            sel_other: 0.9,
            in_subquery_rows: 25.0,

            // Scan/Seek costs
            cpu_cost_per_row: 0.003,
            cpu_cost_per_where_step: 0.003,
            cpu_cost_per_seek: 0.01,
            cache_pages: 2000.0,
            page_miss: 1.0,
            statement_memory: 0,

            // Sort costs
            sort_cpu_per_row: 0.002,

            // Hash join specific costs
            hash_cpu_cost: 0.001,
            hash_insert_cost: 0.002,
            hash_lookup_cost: 0.003,
            hash_bytes_per_row: 100.0,

            plan_values: None,
        }
    }
}

/// Compile-time static default parameters (zero runtime overhead).
#[cfg(any(
    not(feature = "optimizer_params"),
    all(test, feature = "optimizer_params")
))]
pub static DEFAULT_PARAMS: CostModelParams = CostModelParams::new();

impl Default for CostModelParams {
    fn default() -> Self {
        Self::new()
    }
}

impl CostModelParams {
    /// Amphora: these parameters with the connection's cache, `pages` (decision 5).
    pub fn with_cache_pages(&self, pages: usize) -> Self {
        Self {
            cache_pages: pages as f64,
            ..self.clone()
        }
    }

    /// Amphora (PL.7): these parameters with the connection's usable page bytes, where known.
    pub(crate) fn with_page_bytes(self, bytes: Option<usize>) -> Self {
        match bytes {
            Some(bytes) => Self {
                page_bytes: bytes as f64,
                ..self
            },
            None => self,
        }
    }

    /// Amphora (PL.7, T-PL.7.1): these parameters for costing reads of `table`: its rows a page and
    /// its indexes', as ANALYZE counted its trees' pages, else from the page size and its records
    /// (`cost::table_rows_per_page`). A table that is no B-tree keeps them as they are.
    pub(crate) fn for_table(
        &self,
        table: &crate::translate::plan::JoinedTable,
        analyze_stats: &crate::stats::AnalyzeStats,
    ) -> Self {
        let crate::schema::Table::BTree(btree) = &table.table else {
            return self.clone();
        };
        let stats = analyze_stats.table_stats(&btree.name);
        let rows = stats
            .and_then(|s| s.row_count)
            .map_or(self.rows_per_table_fallback, |r| r as f64);
        Self {
            rows_per_table_page: super::cost::table_rows_per_page(
                stats,
                btree.columns().len(),
                rows,
                self.page_bytes,
            ),
            index_rows_per_page: stats
                .map(|s| s.index_rows_per_page.clone())
                .filter(|m| !m.is_empty()),
            ..self.clone()
        }
    }

    /// Amphora (PL.12): these parameters with the statement's parameter values, `values`.
    pub(crate) fn with_plan_values(
        self,
        values: Option<&crate::sync::Arc<crate::plan_cache::PlanValues>>,
    ) -> Self {
        Self {
            plan_values: values.cloned(),
            ..self
        }
    }

    /// Amphora (PL.12): the value an estimate takes for `expr`: a literal's own, a parameter's
    /// bound value (noting that an estimate asked for it), else `None`.
    pub(crate) fn constant_value(
        &self,
        expr: &amphora_engine_parser::ast::Expr,
    ) -> Option<crate::Value> {
        use amphora_engine_parser::ast;
        let mut expr = expr;
        // A parenthesis of one expression is that expression: walked in a loop, not by recursion
        // (CLAUDE.md §11); the AST is finite, so the loop ends.
        while let ast::Expr::Parenthesized(exprs) = expr {
            let [only] = exprs.as_slice() else {
                return None;
            };
            expr = only;
        }
        match expr {
            ast::Expr::Literal(ast::Literal::Numeric(n)) => match n.parse::<i64>() {
                Ok(i) => Some(crate::Value::from_i64(i)),
                Err(_) => n.parse::<f64>().ok().map(crate::Value::from_f64),
            },
            ast::Expr::Literal(ast::Literal::String(s)) => Some(crate::Value::build_text(
                crate::translate::expr::sanitize_string(s),
            )),
            ast::Expr::Variable(variable) => {
                let values = self.plan_values.as_deref()?;
                values.note_asked();
                values.value(variable).cloned()
            }
            _ => None,
        }
    }

    /// Amphora (QT.5): these parameters with the seek's, the hash table's build and probe and the
    /// sort's weights learned where the engine runs (`learned_costs`), each its multiple of a row's
    /// visit times `cpu_cost_per_row`; and `cpu_cost_per_row` itself, the row's visit against a
    /// page read, once the database has timed one. The learned build and probe times include
    /// hashing the key, so `hash_cpu_cost` adds nothing to them. None, a clock that never moved,
    /// leaves these.
    pub(crate) fn with_learned(
        &self,
        learned: Option<super::learned_costs::LearnedWeights>,
    ) -> Self {
        let Some(learned) = learned else {
            return self.clone();
        };
        let row = learned.row_in_pages.unwrap_or(self.cpu_cost_per_row);
        Self {
            cpu_cost_per_row: row,
            cpu_cost_per_seek: row * learned.seek,
            hash_cpu_cost: 0.0,
            hash_insert_cost: row * learned.hash_build,
            hash_lookup_cost: row * learned.hash_probe,
            sort_cpu_per_row: row * learned.sort,
            page_miss: learned.page_miss.unwrap_or(1.0),
            ..self.clone()
        }
    }

    /// Amphora (PL.7): these parameters with the weights fitted to the database's measured
    /// statement times (`cost_fit`), read after the learned ones: each fitted weight in page reads,
    /// and each weight with no samples as it stands (the learned calibration's). None, the switch
    /// off or nothing fitted yet, leaves these.
    pub(crate) fn with_fitted(&self, fitted: Option<super::cost_fit::FittedWeights>) -> Self {
        use super::cost_fit::Term;
        let Some(f) = fitted else {
            return self.clone();
        };
        let row = f.in_pages(Term::Row, self.cpu_cost_per_row, self.cpu_cost_per_row);
        Self {
            cpu_cost_per_row: row,
            cpu_cost_per_seek: f.in_pages(Term::Seek, self.cpu_cost_per_seek, row),
            hash_insert_cost: f.in_pages(Term::HashBuild, self.hash_insert_cost, row),
            hash_lookup_cost: f.in_pages(Term::HashProbe, self.hash_lookup_cost, row),
            sort_cpu_per_row: f.in_pages(Term::Sort, self.sort_cpu_per_row, row),
            ..self.clone()
        }
    }

    /// Amphora: these parameters with the statement's memory, `bytes` (executor decision 8).
    pub fn with_statement_memory(&self, bytes: usize) -> Self {
        Self {
            statement_memory: bytes,
            ..self.clone()
        }
    }
}

impl CostModelParams {
    /// Load parameters from a JSON file.
    ///
    /// Returns default parameters if the file cannot be read, parsed, or validated.
    #[cfg(feature = "optimizer_params")]
    pub fn load_from_file(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str::<Self>(&contents) {
                Ok(params) => {
                    if let Err(e) = params.validate() {
                        tracing::warn!(?path, error = %e, "Invalid cost params, using defaults");
                        return Self::default();
                    }
                    tracing::info!(?path, "Loaded optimizer cost parameters from file");
                    params
                }
                Err(e) => {
                    tracing::warn!(?path, error = %e, "Failed to parse cost params JSON, using defaults");
                    Self::default()
                }
            },
            Err(e) => {
                tracing::warn!(?path, error = %e, "Failed to read cost params file, using defaults");
                Self::default()
            }
        }
    }

    /// Load parameters from the `TURSO_OPTIMIZER_PARAMS` environment variable path,
    /// or return defaults if not set or loading fails.
    #[cfg(feature = "optimizer_params")]
    fn from_env_or_default() -> Self {
        match std::env::var("TURSO_OPTIMIZER_PARAMS") {
            Ok(path) => Self::load_from_file(std::path::Path::new(&path)),
            Err(_) => Self::default(),
        }
    }
}

/// Lazily-loaded parameters from `TURSO_OPTIMIZER_PARAMS` env var (cached process-wide).
/// Falls back to defaults if env var not set or loading fails.
#[cfg(feature = "optimizer_params")]
pub static LOADED_PARAMS: std::sync::LazyLock<CostModelParams> =
    std::sync::LazyLock::new(CostModelParams::from_env_or_default);

#[cfg(feature = "optimizer_params")]
impl CostModelParams {
    /// Validate that parameters are within sensible bounds.
    ///
    /// Returns an error message if any parameter is invalid.
    #[cfg(feature = "optimizer_params")]
    pub fn validate(&self) -> Result<(), String> {
        // Selectivity must be in (0, 1]
        let selectivity_params = [
            ("sel_eq_unindexed", self.sel_eq_unindexed),
            ("sel_eq_indexed", self.sel_eq_indexed),
            ("sel_range", self.sel_range),
            ("sel_closed_range", self.sel_closed_range),
            ("sel_is_null", self.sel_is_null),
            ("sel_is_not_null", self.sel_is_not_null),
            ("sel_like", self.sel_like),
            ("sel_not_like", self.sel_not_like),
            ("sel_other", self.sel_other),
        ];

        for (name, val) in selectivity_params {
            if val <= 0.0 || val > 1.0 {
                return Err(format!("{name} must be in (0, 1], got {val}"));
            }
        }

        // Indexed selectivity should be <= unindexed (indexes are more selective)
        if self.sel_eq_indexed > self.sel_eq_unindexed {
            return Err(format!(
                "sel_eq_indexed ({}) should be <= sel_eq_unindexed ({})",
                self.sel_eq_indexed, self.sel_eq_unindexed
            ));
        }

        // Positive value checks
        if self.rows_per_table_fallback <= 0.0 {
            return Err("rows_per_table_fallback must be positive".into());
        }
        if self.rows_per_table_page <= 0.0 {
            return Err("rows_per_table_page must be positive".into());
        }
        if self.in_subquery_rows <= 0.0 {
            return Err("in_subquery_rows must be positive".into());
        }

        if self.cache_pages < 1.0 {
            return Err("cache_pages must be at least 1".into());
        }

        // Cost multipliers must be non-negative
        let cost_params = [
            ("cpu_cost_per_row", self.cpu_cost_per_row),
            ("cpu_cost_per_where_step", self.cpu_cost_per_where_step),
            ("cpu_cost_per_seek", self.cpu_cost_per_seek),
            ("sort_cpu_per_row", self.sort_cpu_per_row),
            ("hash_cpu_cost", self.hash_cpu_cost),
            ("hash_insert_cost", self.hash_insert_cost),
            ("hash_lookup_cost", self.hash_lookup_cost),
        ];

        for (name, val) in cost_params {
            if val < 0.0 {
                return Err(format!("{name} must be non-negative, got {val}"));
            }
        }

        Ok(())
    }
}

#[cfg(all(test, feature = "optimizer_params"))]
mod tests {
    use super::*;

    #[test]
    fn test_default_params_are_valid() {
        let params = CostModelParams::default();
        assert!(params.validate().is_ok());
    }

    #[test]
    fn test_invalid_selectivity_rejected() {
        let mut params = CostModelParams {
            sel_eq_unindexed: 1.5,
            ..Default::default()
        };
        assert!(params.validate().is_err());

        params = CostModelParams {
            sel_range: 0.0,
            ..Default::default()
        };
        assert!(params.validate().is_err());

        params = CostModelParams {
            sel_is_null: -0.1,
            ..Default::default()
        };
        assert!(params.validate().is_err());
    }

    #[test]
    fn test_indexed_selectivity_constraint() {
        let params = CostModelParams {
            sel_eq_indexed: 0.5,
            sel_eq_unindexed: 0.1,
            ..Default::default()
        };
        assert!(params.validate().is_err());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_serde_roundtrip() {
        let params = CostModelParams::default();
        let json = serde_json::to_string(&params).unwrap();
        let parsed: CostModelParams = serde_json::from_str(&json).unwrap();
        assert!((params.sel_eq_unindexed - parsed.sel_eq_unindexed).abs() < f64::EPSILON);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_partial_json_uses_defaults() {
        let defaults = CostModelParams::new();
        let json = r#"{"sel_eq_unindexed": 0.05}"#;
        let params: CostModelParams = serde_json::from_str(json).unwrap();
        assert!((params.sel_eq_unindexed - 0.05).abs() < f64::EPSILON);
        // Other fields should be defaults
        assert!((params.sel_range - defaults.sel_range).abs() < f64::EPSILON);
    }

    #[test]
    fn test_load_from_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_cost_params.json");
        let defaults = CostModelParams::new();

        // Write a partial JSON file - unspecified fields should use defaults
        let json = r#"{
            "sel_eq_unindexed": 0.15,
            "sel_eq_indexed": 0.005,
            "rows_per_table_fallback": 500000.0
        }"#;
        std::fs::write(&path, json).unwrap();

        let params = CostModelParams::load_from_file(&path);

        // Specified values should be loaded
        assert!((params.sel_eq_unindexed - 0.15).abs() < f64::EPSILON);
        assert!((params.sel_eq_indexed - 0.005).abs() < f64::EPSILON);
        assert!((params.rows_per_table_fallback - 500000.0).abs() < f64::EPSILON);

        // Unspecified values should be defaults
        assert!((params.sel_range - defaults.sel_range).abs() < f64::EPSILON);
        assert!((params.rows_per_table_page - defaults.rows_per_table_page).abs() < f64::EPSILON);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_load_from_file_invalid_json_returns_defaults() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_invalid_cost_params.json");
        let defaults = CostModelParams::new();

        std::fs::write(&path, "not valid json {{{").unwrap();

        let params = CostModelParams::load_from_file(&path);

        // Should return defaults on parse error
        assert!((params.sel_eq_unindexed - defaults.sel_eq_unindexed).abs() < f64::EPSILON);
        assert!(
            (params.rows_per_table_fallback - defaults.rows_per_table_fallback).abs()
                < f64::EPSILON
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_load_from_file_missing_returns_defaults() {
        let path = std::path::Path::new("/nonexistent/path/to/params.json");
        let defaults = CostModelParams::new();

        let params = CostModelParams::load_from_file(path);

        // Should return defaults when file doesn't exist
        assert!((params.sel_eq_unindexed - defaults.sel_eq_unindexed).abs() < f64::EPSILON);
        assert!(
            (params.rows_per_table_fallback - defaults.rows_per_table_fallback).abs()
                < f64::EPSILON
        );
    }
}
