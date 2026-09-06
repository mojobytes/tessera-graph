// SPDX-License-Identifier: BSL-1.1

//! Graph access abstraction for query and mutation execution.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use ermya_graph::gql::{self, GqlMutationResult, GqlQuery, GqlValue};
use ermya_graph::Graph;

/// A single result row: column name → value.
pub type ResultRow = std::collections::HashMap<String, GqlValue>;

/// Sentinel prefix used by every executor in this module when the
/// underlying [`ermya_graph::Error`] is a `QuotaExceeded`. The Bolt
/// handler (`handler::error_code_for_engine_msg`) matches this prefix
/// to surface the wire code `Neo.ClientError.General.StorageExhausted`.
///
/// Task 15 design note: the `GraphAccessor` trait returns `Result<_,
/// String>` to keep the public interface stable across current and
/// future accessor implementations. Embedding a sentinel in the error string is a
/// pragmatic compromise — the alternative is propagating
/// `ermya_graph::Error` through the trait, which would touch every
/// downstream implementation. The sentinel is referenced from a
/// single constant on the server side; downstream consumers should
/// never depend on the exact wording of the rest of the message.
pub const ENGINE_QUOTA_EXCEEDED_PREFIX: &str = "__TG_QUOTA_EXCEEDED__: ";

/// Sentinel prefix used when the engine aborted a query via the defensive
/// result-row cap (a `GqlCompileError` carrying
/// [`ermya_graph::gql::RESULT_CAP_MSG_PREFIX`], or the output-row guard
/// in [`enforce_output_cap`]). The Bolt handler matches this prefix to
/// surface the wire code `Neo.ClientError.General.ResultExhausted`.
pub const ENGINE_RESULT_CAPPED_PREFIX: &str = "__TG_RESULT_CAPPED__: ";

/// Sentinel prefix used when the engine aborted a query via the cooperative
/// query-timeout deadline (a `GqlCompileError` carrying
/// [`ermya_graph::gql::TIMEOUT_MSG_PREFIX`]). The Bolt handler matches this
/// prefix to surface the wire code `Neo.ClientError.Statement.ExecutionFailed`
/// — a non-retryable `ClientError`, so the driver does not re-run the same
/// expensive query (v0.6.0 Fase 2 Task 6).
pub const ENGINE_QUERY_TIMEOUT_PREFIX: &str = "__TG_QUERY_TIMEOUT__: ";

/// Sentinel prefix used when a write was rejected by a unique constraint
/// (`ermya_graph::Error::ConstraintViolation`). The Bolt handler matches
/// this prefix to surface `Neo.ClientError.Schema.ConstraintValidationFailed`
/// — the wire code the Neo4j/.NET driver expects for a uniqueness failure.
pub const ENGINE_CONSTRAINT_VIOLATED_PREFIX: &str = "__TG_CONSTRAINT_VIOLATED__: ";

/// Sentinel prefix used when a batch was rejected for exceeding its configured
/// operation-count or byte cap (`ermya_graph::Error::BatchLimitExceeded`).
/// The Bolt handler answers with `Neo.ClientError.Request.Invalid`: unlike the
/// transaction memory cap this is NOT transient — replaying the same oversized
/// batch fails identically, and the client must split it.
pub const ENGINE_BATCH_LIMIT_PREFIX: &str = "__TG_BATCH_LIMIT__: ";

/// Sentinel prefix used when a transaction was aborted for exceeding its
/// per-transaction memory cap (`ermya_graph::Error::TxnMemoryCapExceeded`).
/// The Bolt handler answers with `Neo.TransientError.General.MemoryPoolOutOfMemory`:
/// this one IS worth retrying, because the same work split into smaller
/// transactions can succeed.
///
/// Only the write path needs this. The cap is charged exclusively by the
/// in-transaction mutations (`add`/`update`/`remove` of nodes and edges), which
/// all surface through `RUN`; `commit_txn`/`begin_txn` never charge it, so the
/// `map_txn_error` path cannot produce this variant.
pub const ENGINE_TXN_MEMORY_CAP_PREFIX: &str = "__TG_TXN_MEMORY_CAP__: ";

/// Sentinel prefix used when a node could not be deleted because it still had
/// relationships and the query omitted `DETACH`
/// (`ermya_graph::Error::DeleteConnectedNode`). The Bolt handler matches this
/// prefix to answer with `Neo.ClientError.Schema.ConstraintValidationFailed`,
/// which is how Neo4j reports the same graph-integrity violation.
pub const ENGINE_DELETE_CONNECTED_PREFIX: &str = "__TG_DELETE_CONNECTED__: ";

/// Sentinel prefix used when a write inside an explicit transaction targeted a
/// label declared append-only (`ermya_graph::Error::AppendOnlyLabelInTxn`,
/// issue #43). The Bolt handler matches this prefix to answer with
/// `Neo.ClientError.Request.Invalid` — the client asked for something the
/// server will never do, so retrying verbatim cannot help.
pub const ENGINE_APPEND_ONLY_IN_TXN_PREFIX: &str = "__TG_APPEND_ONLY_IN_TXN__: ";

/// Convert a `ermya_graph::Error` into the `String` form that the
/// `GraphAccessor` trait returns, preserving the `QuotaExceeded` variant
/// via the [`ENGINE_QUOTA_EXCEEDED_PREFIX`] sentinel so the Bolt handler
/// can recover it for wire-code mapping.
// `needless_pass_by_value`: taking ownership lets `map_err` accept
// this fn directly as `.map_err(engine_err_to_string)` rather than
// forcing every call site to wrap with `|e| engine_err_to_string(&e)`.
// The cost is negligible: `Error` is an enum, not a heavyweight struct.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn engine_err_to_string(e: ermya_graph::Error) -> String {
    if matches!(e, ermya_graph::Error::QuotaExceeded { .. }) {
        format!("{ENGINE_QUOTA_EXCEEDED_PREFIX}{e}")
    } else if matches!(e, ermya_graph::Error::ConstraintViolation { .. }) {
        format!("{ENGINE_CONSTRAINT_VIOLATED_PREFIX}{e}")
    } else if matches!(e, ermya_graph::Error::AppendOnlyLabelInTxn { .. }) {
        format!("{ENGINE_APPEND_ONLY_IN_TXN_PREFIX}{e}")
    } else if matches!(e, ermya_graph::Error::DeleteConnectedNode { .. }) {
        format!("{ENGINE_DELETE_CONNECTED_PREFIX}{e}")
    } else if matches!(e, ermya_graph::Error::TxnMemoryCapExceeded { .. }) {
        format!("{ENGINE_TXN_MEMORY_CAP_PREFIX}{e}")
    } else if matches!(e, ermya_graph::Error::BatchLimitExceeded { .. }) {
        format!("{ENGINE_BATCH_LIMIT_PREFIX}{e}")
    } else if matches!(&e, ermya_graph::Error::GqlCompileError(m)
        if m.starts_with(ermya_graph::gql::RESULT_CAP_MSG_PREFIX))
    {
        format!("{ENGINE_RESULT_CAPPED_PREFIX}{e}")
    } else if matches!(&e, ermya_graph::Error::GqlCompileError(m)
        if m.starts_with(ermya_graph::gql::TIMEOUT_MSG_PREFIX))
    {
        format!("{ENGINE_QUERY_TIMEOUT_PREFIX}{e}")
    } else {
        e.to_string()
    }
}

/// Enforce the output-row cap (Cap B) over a materialized result set at the
/// `GraphAccessor` boundary. Single DRY enforcement point covering every
/// engine exit path (UNWIND, aggregate pushdown, GROUP BY, pipeline) — the
/// match-count guard (Cap A) inside `gql::execute` only sees pre-projection
/// match counts. `0` disables the cap. The error carries
/// [`ENGINE_RESULT_CAPPED_PREFIX`] so the handler maps it to
/// `Neo.ClientError.General.ResultExhausted`.
fn enforce_output_cap(rows: Vec<ResultRow>, max_rows: u64) -> Result<Vec<ResultRow>, String> {
    if max_rows > 0 && rows.len() as u64 > max_rows {
        return Err(format!(
            "{ENGINE_RESULT_CAPPED_PREFIX}query produced {} rows, exceeds max_result_rows={max_rows}",
            rows.len()
        ));
    }
    Ok(rows)
}

/// Extension point for graph access.
///
/// The Community server uses [`DefaultGraphAccessor`] which executes queries
/// directly against `Arc<RwLock<Graph>>`. Enterprise can implement this
/// trait to wrap access with LBAC, audit logging, and tenant isolation.
///
/// # `params` ownership
///
/// All execution methods take `params: HashMap<String, GqlValue>` **by
/// value**, not by reference. The handler builds the map once from the
/// Bolt `RUN.params` field, applies substitution to the AST, and then
/// moves the map into the accessor call. Enterprise implementations that
/// need to inspect the map (audit log, LBAC) own it for the duration of
/// the call without an extra clone; the Community default impl ignores it and
/// the move is zero-cost. A `&HashMap<...>` signature was considered but
/// would force enterprise auditors to clone the map every call.
pub trait GraphAccessor: Send + Sync + 'static {
    /// Execute a read-only query, returning rows.
    ///
    /// `params` carries the **original** Bolt `RUN` parameter values. The
    /// caller is expected to have already applied
    /// [`ermya_graph::gql::param_substitution::apply`] to `query`, so the
    /// AST reaching this method contains no `Expr::ParamRef`. The map is
    /// passed through to let enterprise implementors inspect or log the
    /// original bindings (LBAC, audit). Callers that do not need
    /// parametrisation (CLI, internal tests) pass `HashMap::new()`.
    ///
    /// # Errors
    ///
    /// Returns an error string on failure.
    /// `deadline` (v0.6.0 Fase 2 Task 6) is the cooperative query-timeout
    /// instant; `None` disables the engine's deadline checks. On expiry the
    /// engine aborts and the error carries [`ENGINE_QUERY_TIMEOUT_PREFIX`].
    fn execute_query(
        &self,
        query: &GqlQuery,
        params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<Vec<ResultRow>, String>;

    /// Execute a mutation (CREATE, DELETE, SET, MERGE).
    ///
    /// Returns `(rows, nodes_created, edges_created)` on success. `rows` is
    /// empty for non-returning mutations (CREATE / SET / DELETE and bare
    /// MERGE); it carries the projected result for `MERGE (...) RETURN var`
    /// (a single row mapping `var` → the merged node as a `GqlValue::Map`).
    /// `params` has the same semantics as in [`Self::execute_query`].
    ///
    /// `deadline` bounds the **MATCH phase only** — the write/commit phase
    /// never receives a deadline, so no mutation is ever cut mid-write
    /// (Task 6 design decision #6).
    ///
    /// # Errors
    ///
    /// Returns an error string on failure.
    fn execute_mutation(
        &self,
        mutation: &gql::MutationStatement,
        params: HashMap<String, GqlValue>,
        deadline: Option<Instant>,
    ) -> Result<(Vec<ResultRow>, GqlMutationResult), String>;

    /// Execute a pipeline statement (`MATCH ... WITH ... RETURN|SET|CREATE|DELETE`).
    ///
    /// Read-only pipelines (`... RETURN ...`) return the projected rows in the
    /// first tuple element and a default (all-zero) [`GqlMutationResult`] in the
    /// second. Mutation pipelines (`... SET / CREATE / DELETE`) return no rows
    /// and the populated counts. `params` has the same semantics as in
    /// [`Self::execute_query`].
    ///
    /// # Errors
    ///
    /// Returns an error string on parse, compile, or runtime failure.
    fn execute_pipeline(
        &self,
        pq: &gql::PipelineQuery,
        params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<(Vec<ResultRow>, GqlMutationResult), String>;

    /// Execute a `RETURN <expr-list>` root statement against an empty
    /// binding context. Produces exactly one row, or zero rows when
    /// `SKIP >= 1` or `LIMIT == 0`.
    ///
    /// `params` has the same semantics as in [`Self::execute_query`].
    /// No transaction is opened. No buffer-pool page is touched (constant
    /// expressions are evaluated against `PatternMatch::empty()`).
    ///
    /// # Errors
    ///
    /// Returns an error string on compile or runtime failure.
    fn execute_const_return(
        &self,
        q: &gql::ConstReturnQuery,
        params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<Vec<ResultRow>, String>;

    /// Execute a read-only query **inside** the explicit transaction `txn_id`,
    /// so it sees the transaction's own uncommitted writes (read-your-writes).
    ///
    /// Semantics match [`Self::execute_query`]; the only difference is the MVCC
    /// snapshot the read runs against. The default implementation runs the
    /// lookup under a **read lock** over a transaction-scoped read-only view,
    /// preserving the two-lock discipline of the auto-commit path — a costly
    /// MATCH inside a transaction never stalls concurrent auto-commit readers.
    ///
    /// # Errors
    ///
    /// Returns an error string on failure, including if `txn_id` is not active.
    fn execute_query_in_txn(
        &self,
        txn_id: u64,
        query: &GqlQuery,
        params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<Vec<ResultRow>, String>;

    /// Execute a mutation **inside** the explicit transaction `txn_id`. The
    /// write is recorded as a transaction delta (invisible to other sessions
    /// until `COMMIT`) and is visible to later reads in the same transaction.
    ///
    /// Semantics and return shape match [`Self::execute_mutation`]. The write
    /// delegates to the engine's unified mutation path with `Some(txn_id)`; the
    /// lookup phase (MATCH/MERGE/UNWIND) runs under a read lock over a
    /// transaction-scoped read-only view, the apply phase under a write lock —
    /// the same two-lock discipline as auto-commit.
    ///
    /// # Errors
    ///
    /// Returns an error string on failure, including if `txn_id` is not active.
    fn execute_mutation_in_txn(
        &self,
        txn_id: u64,
        mutation: &gql::MutationStatement,
        params: HashMap<String, GqlValue>,
        deadline: Option<Instant>,
    ) -> Result<(Vec<ResultRow>, GqlMutationResult), String>;

    /// Execute a pipeline statement **inside** the explicit transaction
    /// `txn_id`. Read-only pipelines run against the transaction snapshot;
    /// `SET`-terminal pipelines record their writes as transaction deltas.
    ///
    /// Semantics and return shape match [`Self::execute_pipeline`].
    ///
    /// # Errors
    ///
    /// Returns an error string on failure, including if `txn_id` is not active.
    fn execute_pipeline_in_txn(
        &self,
        txn_id: u64,
        pq: &gql::PipelineQuery,
        params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<(Vec<ResultRow>, GqlMutationResult), String>;

    /// Execute a `RETURN <expr-list>` root statement inside the explicit
    /// transaction `txn_id`. Constant expressions touch no graph state, so this
    /// is a passthrough to the transaction-agnostic evaluation; the `txn_id`
    /// parameter exists only for dispatch uniformity with the other
    /// `*_in_txn` methods.
    ///
    /// # Errors
    ///
    /// Returns an error string on compile or runtime failure.
    fn execute_const_return_in_txn(
        &self,
        txn_id: u64,
        q: &gql::ConstReturnQuery,
        params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<Vec<ResultRow>, String>;

    /// Enter batch mode — defers WAL sync until [`end_batch`](Self::end_batch).
    ///
    /// # Errors
    ///
    /// Returns an error string on failure.
    fn begin_batch(&self) -> Result<(), String>;

    /// Exit batch mode — issues a single WAL sync for all coalesced mutations.
    ///
    /// # Errors
    ///
    /// Returns an error string on failure.
    fn end_batch(&self) -> Result<(), String>;

    /// Opens an explicit MVCC transaction and returns its `txn_id`. The Bolt
    /// handler stores it as the session's open transaction so subsequent `RUN`s
    /// execute inside it until `COMMIT`/`ROLLBACK`.
    ///
    /// # Errors
    ///
    /// Returns an error string if MVCC is not enabled or the engine rejects the
    /// begin.
    fn begin_txn(&self) -> Result<u64, String>;

    /// Commits the explicit transaction `txn_id`, making its writes visible.
    ///
    /// # Errors
    ///
    /// Returns an error string if `txn_id` is not active or the commit fails.
    fn commit_txn(&self, txn_id: u64) -> Result<(), String>;

    /// Rolls back the explicit transaction `txn_id`, discarding its writes.
    ///
    /// # Errors
    ///
    /// Returns an error string if `txn_id` is not active or the rollback fails.
    fn rollback_txn(&self, txn_id: u64) -> Result<(), String>;

    /// Returns the underlying `Arc<RwLock<Graph>>` for DDL statements that
    /// need direct access to the schema catalog living on the [`Graph`].
    ///
    /// DDL (CREATE/DROP INDEX/CONSTRAINT, SHOW) mutates or reads the
    /// `SchemaCatalog`, which is not reachable through the query/mutation
    /// methods above. The Bolt handler resolves the session's selected
    /// database to its graph via this method and dispatches DDL against it,
    /// keeping per-tenant catalogs isolated.
    fn graph_arc(&self) -> Arc<RwLock<Graph>>;
}

/// Direct graph access without security wrappers.
pub struct DefaultGraphAccessor {
    graph: Arc<RwLock<Graph>>,
}

impl DefaultGraphAccessor {
    /// Wrap a shared graph handle.
    #[must_use]
    pub fn new(graph: Arc<RwLock<Graph>>) -> Self {
        Self { graph }
    }
}

impl GraphAccessor for DefaultGraphAccessor {
    fn execute_query(
        &self,
        query: &GqlQuery,
        _params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<Vec<ResultRow>, String> {
        let graph = self
            .graph
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        let rows = gql::execute_with_deadline(&*graph, query, max_rows, deadline)
            .map_err(engine_err_to_string)?;
        enforce_output_cap(rows, max_rows)
    }

    fn execute_mutation(
        &self,
        mutation: &gql::MutationStatement,
        params: HashMap<String, GqlValue>,
        deadline: Option<Instant>,
    ) -> Result<(Vec<ResultRow>, GqlMutationResult), String> {
        // UNWIND+CREATE path: delegate to specialized executor.
        if mutation.unwind_clause.is_some() {
            return execute_unwind_mutation(&self.graph, mutation, deadline)
                .map(|stats| (Vec::new(), stats));
        }
        if mutation.match_clause.is_some() {
            // MATCH…CREATE / MATCH…SET path: compile bindings with read lock,
            // then mutate. The deadline bounds the MATCH phase only (decision
            // #6); the write phase below never receives it, so no write is cut
            // mid-flight. `params` carries `$map` values that survive
            // `param_substitution` as unsubstituted `ParamRef`s (there is no
            // `Literal::Map`), so the SET branch resolves them from here.
            return execute_match_mutation(&self.graph, mutation, &params, deadline);
        }
        // MERGE path: own lock management (read-then-write), may return rows.
        if let gql::MutationClause::Merge(merge) = &mutation.mutation {
            return execute_bare_merge(&self.graph, merge, &params);
        }
        // (fall through to bare CREATE)
        let mut graph = self
            .graph
            .write()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        gql::execute_bare_mutation(&mut graph, mutation, &params, None)
            .map_err(engine_err_to_string)
    }

    fn execute_pipeline(
        &self,
        pq: &gql::PipelineQuery,
        _params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<(Vec<ResultRow>, GqlMutationResult), String> {
        use ermya_graph::gql::PipelineTerminal;

        match &pq.terminal {
            PipelineTerminal::Return { .. } => {
                let graph = self
                    .graph
                    .read()
                    .map_err(|_| "graph lock poisoned".to_owned())?;
                let rows = gql::execute_pipeline_with_deadline(&*graph, pq, max_rows, deadline)
                    .map_err(engine_err_to_string)?;
                Ok((
                    enforce_output_cap(rows, max_rows)?,
                    GqlMutationResult::default(),
                ))
            }
            PipelineTerminal::Set(_) | PipelineTerminal::Delete(_) => {
                // SET / DELETE terminals: the engine's pipeline mutation runs
                // the read-only stages, then applies the write under a write
                // lock (auto-commit: `txn_id = None`).
                let mut graph = self
                    .graph
                    .write()
                    .map_err(|_| "graph lock poisoned".to_owned())?;
                let stmt = gql::GqlStatement::Pipeline(pq.clone());
                let result = gql::execute_pipeline_mutation(&mut graph, &stmt, None)
                    .map_err(engine_err_to_string)?;
                Ok((Vec::new(), result))
            }
            PipelineTerminal::Create(_) => {
                Err("CREATE pipeline terminal is not yet supported".to_owned())
            }
        }
    }

    fn execute_const_return(
        &self,
        q: &gql::ConstReturnQuery,
        _params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<Vec<ResultRow>, String> {
        // Read lock is sufficient: ConstReturn touches no graph state, but
        // we acquire it for two reasons: (1) `eval_expr` borrows
        // `&G: GraphAccess` even when the expression is constant, and (2)
        // it serialises against ongoing writes so a slow constant
        // expression cannot read torn state — a non-issue today (no graph
        // access path inside ConstReturn) but cheap insurance.
        let graph = self
            .graph
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        // const return is always one row; max_rows and deadline are no-ops but
        // threaded through for trait uniformity (and forwarded to the engine).
        gql::execute_const_return(&*graph, q, max_rows, deadline).map_err(engine_err_to_string)
    }

    fn execute_query_in_txn(
        &self,
        txn_id: u64,
        query: &GqlQuery,
        _params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<Vec<ResultRow>, String> {
        // Read lock only: the read-only txn view reads the transaction's
        // snapshot (its own pending writes + committed base) via `&self`
        // engine methods, so no write lock is taken — auto-commit readers are
        // never blocked by a transactional MATCH.
        let graph = self
            .graph
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        let view = gql::TxnReadView::new(&graph, txn_id);
        let rows = gql::execute_with_deadline(&view, query, max_rows, deadline)
            .map_err(engine_err_to_string)?;
        enforce_output_cap(rows, max_rows)
    }

    fn execute_mutation_in_txn(
        &self,
        txn_id: u64,
        mutation: &gql::MutationStatement,
        params: HashMap<String, GqlValue>,
        deadline: Option<Instant>,
    ) -> Result<(Vec<ResultRow>, GqlMutationResult), String> {
        // UNWIND+CREATE path.
        if mutation.unwind_clause.is_some() {
            return execute_unwind_mutation_in_txn(&self.graph, txn_id, mutation, deadline)
                .map(|stats| (Vec::new(), stats));
        }
        // MATCH…CREATE / MATCH…SET path.
        if mutation.match_clause.is_some() {
            return execute_match_mutation_in_txn(&self.graph, txn_id, mutation, &params, deadline);
        }
        // MERGE path.
        if let gql::MutationClause::Merge(merge) = &mutation.mutation {
            return execute_bare_merge_in_txn(&self.graph, txn_id, merge, &params);
        }
        // Bare CREATE: single write phase, no lookup — take the write lock and
        // delegate to the engine's unified write path with `Some(txn_id)`.
        let mut graph = self
            .graph
            .write()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        gql::execute_bare_mutation(&mut graph, mutation, &params, Some(txn_id))
            .map_err(engine_err_to_string)
    }

    fn execute_pipeline_in_txn(
        &self,
        txn_id: u64,
        pq: &gql::PipelineQuery,
        _params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<(Vec<ResultRow>, GqlMutationResult), String> {
        use ermya_graph::gql::PipelineTerminal;

        match &pq.terminal {
            PipelineTerminal::Return { .. } => {
                // Read-only pipeline: read lock over the txn snapshot view.
                let graph = self
                    .graph
                    .read()
                    .map_err(|_| "graph lock poisoned".to_owned())?;
                let view = gql::TxnReadView::new(&graph, txn_id);
                let rows = gql::execute_pipeline_with_deadline(&view, pq, max_rows, deadline)
                    .map_err(engine_err_to_string)?;
                Ok((
                    enforce_output_cap(rows, max_rows)?,
                    GqlMutationResult::default(),
                ))
            }
            PipelineTerminal::Set(_) | PipelineTerminal::Delete(_) => {
                // SET / DELETE terminal: the engine's pipeline mutation runs the
                // read-only stages over a txn view and applies the write via the
                // `*_in_txn` primitives when `txn_id` is `Some`. It needs a
                // write lock for the apply phase.
                let mut graph = self
                    .graph
                    .write()
                    .map_err(|_| "graph lock poisoned".to_owned())?;
                let stmt = gql::GqlStatement::Pipeline(pq.clone());
                let result = gql::execute_pipeline_mutation(&mut graph, &stmt, Some(txn_id))
                    .map_err(engine_err_to_string)?;
                Ok((Vec::new(), result))
            }
            PipelineTerminal::Create(_) => {
                Err("CREATE pipeline terminal is not yet supported".to_owned())
            }
        }
    }

    fn execute_const_return_in_txn(
        &self,
        _txn_id: u64,
        q: &gql::ConstReturnQuery,
        _params: HashMap<String, GqlValue>,
        max_rows: u64,
        deadline: Option<Instant>,
    ) -> Result<Vec<ResultRow>, String> {
        // Constant expressions touch no graph state, so the transaction snapshot
        // is irrelevant — this is identical to the auto-commit path. A read lock
        // is taken for the same reasons documented on `execute_const_return`.
        let graph = self
            .graph
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        gql::execute_const_return(&*graph, q, max_rows, deadline).map_err(engine_err_to_string)
    }

    fn begin_batch(&self) -> Result<(), String> {
        let mut graph = self
            .graph
            .write()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        graph.begin_batch();
        Ok(())
    }

    fn end_batch(&self) -> Result<(), String> {
        let mut graph = self
            .graph
            .write()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        graph.end_batch().map_err(engine_err_to_string)
    }

    fn begin_txn(&self) -> Result<u64, String> {
        let mut graph = self
            .graph
            .write()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        graph.begin_txn().map_err(engine_err_to_string)
    }

    fn commit_txn(&self, txn_id: u64) -> Result<(), String> {
        let mut graph = self
            .graph
            .write()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        graph.commit_txn(txn_id).map_err(engine_err_to_string)
    }

    fn rollback_txn(&self, txn_id: u64) -> Result<(), String> {
        let mut graph = self
            .graph
            .write()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        graph.rollback_txn(txn_id).map_err(engine_err_to_string)
    }

    fn graph_arc(&self) -> Arc<RwLock<Graph>> {
        Arc::clone(&self.graph)
    }
}

// ── MATCH…CREATE execution ───────────────────────────────────────────────────

/// Executes a mutation that has a preceding MATCH clause.
///
/// The borrow checker requires two distinct lock acquisitions: an immutable
/// read for the MATCH phase (collecting all bindings into owned data), followed
/// by a mutable write for the CREATE phase. This avoids holding a `&Graph`
/// while calling `&mut Graph` methods.
// `pub(crate)` (not private) so the lock-contention benchmark harness
// (`bench_support`, gated behind the `bench-support` feature) times the real
// production mutation path rather than a copy. No behaviour change.
pub(crate) fn execute_match_mutation(
    shared: &Arc<RwLock<Graph>>,
    mutation: &gql::MutationStatement,
    params: &HashMap<String, GqlValue>,
    deadline: Option<Instant>,
) -> Result<(Vec<ResultRow>, GqlMutationResult), String> {
    let match_clause = mutation
        .match_clause
        .as_ref()
        .ok_or_else(|| "execute_match_mutation invoked without a MATCH clause".to_owned())?;

    // Phase 1 — compile bindings under read lock; all owned data collected.
    // The deadline bounds this MATCH phase; Phase 2 (the write below) runs
    // without it so a mutation is never cut mid-write (Task 6 decision #6).
    let rows: Vec<gql::MatchRow> = {
        let graph = shared
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        gql::compile_match_rows_for_mutation(
            &*graph,
            match_clause,
            mutation.where_clause.as_ref(),
            deadline,
        )
        .map_err(engine_err_to_string)?
    };

    if rows.is_empty() {
        return Ok((Vec::new(), GqlMutationResult::default()));
    }

    // Phase 2 — apply mutations under write lock, delegating to the engine's
    // unified write path (auto-commit: `txn_id = None`).
    let mut graph = shared
        .write()
        .map_err(|_| "graph lock poisoned".to_owned())?;

    gql::apply_match_mutation_body(&mut graph, mutation, &rows, params, None)
        .map_err(engine_err_to_string)
}

/// Transactional twin of [`execute_match_mutation`]: the MATCH phase reads the
/// transaction's own uncommitted writes via a read-only txn view under a read
/// lock, and the CREATE/SET phase records deltas via the engine's unified write
/// path with `Some(txn_id)` under a write lock. Same two-lock discipline.
fn execute_match_mutation_in_txn(
    shared: &Arc<RwLock<Graph>>,
    txn_id: u64,
    mutation: &gql::MutationStatement,
    params: &HashMap<String, GqlValue>,
    deadline: Option<Instant>,
) -> Result<(Vec<ResultRow>, GqlMutationResult), String> {
    let match_clause = mutation
        .match_clause
        .as_ref()
        .ok_or_else(|| "execute_match_mutation_in_txn invoked without a MATCH clause".to_owned())?;

    // Phase 1 — compile bindings under a read lock over the txn snapshot view.
    let rows: Vec<gql::MatchRow> = {
        let graph = shared
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        let view = gql::TxnReadView::new(&graph, txn_id);
        gql::compile_match_rows_for_mutation(
            &view,
            match_clause,
            mutation.where_clause.as_ref(),
            deadline,
        )
        .map_err(engine_err_to_string)?
    };

    if rows.is_empty() {
        return Ok((Vec::new(), GqlMutationResult::default()));
    }

    // Phase 2 — apply mutations under a write lock, delegating to the engine's
    // unified write path scoped to this transaction.
    let mut graph = shared
        .write()
        .map_err(|_| "graph lock poisoned".to_owned())?;

    gql::apply_match_mutation_body(&mut graph, mutation, &rows, params, Some(txn_id))
        .map_err(engine_err_to_string)
}

// ── Bare mutation (no MATCH) ─────────────────────────────────────────────────

// ── MERGE execution ──────────────────────────────────────────────────────────

/// Executes a bare `MERGE …` clause, preserving the two-lock discipline.
///
/// The lookup (create-or-match decision) runs under a shared read lock, which is
/// released before an exclusive write lock is taken for the create/apply phase —
/// the same read-then-write discipline as `execute_match_mutation`, so a MERGE
/// never holds a write lock during its lookup. Both phases delegate to the
/// engine's unified MERGE logic ([`gql::merge_lookup`] / [`gql::apply_merge_write`]);
/// auto-commit passes `txn_id = None`.
fn execute_bare_merge(
    shared: &Arc<RwLock<Graph>>,
    merge: &gql::MergeClause,
    params: &HashMap<String, GqlValue>,
) -> Result<(Vec<ResultRow>, GqlMutationResult), String> {
    // Phase 1 — lookup under a read lock, released at the end of this scope.
    let lookup = {
        let graph = shared
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        gql::merge_lookup(&*graph, merge)
    };

    // Phase 2 — create-or-apply under a write lock.
    let mut graph = shared
        .write()
        .map_err(|_| "graph lock poisoned".to_owned())?;
    gql::apply_merge_write(&mut graph, merge, lookup, params, None).map_err(engine_err_to_string)
}

/// Transactional twin of [`execute_bare_merge`]: the lookup runs over a
/// read-only txn view (so a repeated MERGE in the same transaction finds its own
/// pending node) under a read lock, and the create/apply phase records deltas
/// with `Some(txn_id)` under a write lock. Same read-then-write discipline.
fn execute_bare_merge_in_txn(
    shared: &Arc<RwLock<Graph>>,
    txn_id: u64,
    merge: &gql::MergeClause,
    params: &HashMap<String, GqlValue>,
) -> Result<(Vec<ResultRow>, GqlMutationResult), String> {
    // Phase 1 — lookup under a read lock over the txn snapshot view.
    let lookup = {
        let graph = shared
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        let view = gql::TxnReadView::new(&graph, txn_id);
        gql::merge_lookup(&view, merge)
    };

    // Phase 2 — create-or-apply under a write lock, scoped to this transaction.
    let mut graph = shared
        .write()
        .map_err(|_| "graph lock poisoned".to_owned())?;
    gql::apply_merge_write(&mut graph, merge, lookup, params, Some(txn_id))
        .map_err(engine_err_to_string)
}

// ── UNWIND…CREATE execution ──────────────────────────────────────────────────

/// Executes an `UNWIND … CREATE` mutation, preserving the two-lock discipline.
///
/// Phase 1 (evaluate the UNWIND list and compile MATCH bindings) runs under a
/// shared read lock, released before Phase 2 (the CREATE writes) takes an
/// exclusive write lock — the same read-then-write discipline as the other
/// mutation paths. Both phases delegate to the engine's unified UNWIND logic
/// ([`gql::eval_unwind_and_match`] / [`gql::apply_unwind_create_body`]);
/// auto-commit passes `txn_id = None`.
fn execute_unwind_mutation(
    shared: &Arc<RwLock<Graph>>,
    mutation: &gql::MutationStatement,
    deadline: Option<Instant>,
) -> Result<GqlMutationResult, String> {
    use ermya_graph::gql::MutationClause;

    let unwind = mutation
        .unwind_clause
        .as_ref()
        .ok_or_else(|| "execute_unwind_mutation invoked without an UNWIND clause".to_owned())?;

    // UNWIND supports CREATE and DELETE; reject other clauses early.
    match &mutation.mutation {
        MutationClause::Create(_) | MutationClause::Delete(_) => {}
        other => {
            return Err(format!(
                "mutation clause not yet supported with UNWIND: {other:?}"
            ));
        }
    }

    // Phase 1 — read under a read lock, released at the end of this scope.
    let (elements, rows) = {
        let graph = shared
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        gql::eval_unwind_and_match(&*graph, mutation, unwind, deadline)
            .map_err(engine_err_to_string)?
    };

    if elements.is_empty() || rows.is_empty() {
        return Ok(GqlMutationResult::default());
    }

    // Phase 2 — write under a write lock.
    let mut graph = shared
        .write()
        .map_err(|_| "graph lock poisoned".to_owned())?;
    match &mutation.mutation {
        MutationClause::Create(create) => {
            gql::apply_unwind_create_body(&mut graph, unwind, create, &elements, &rows, None)
                .map_err(engine_err_to_string)
        }
        MutationClause::Delete(dc) => {
            gql::apply_unwind_delete_body(&mut graph, &rows, dc, None).map_err(engine_err_to_string)
        }
        other => Err(format!(
            "mutation clause not yet supported with UNWIND: {other:?}"
        )),
    }
}

/// Transactional twin of [`execute_unwind_mutation`]: Phase 1 (evaluate the
/// UNWIND list and compile MATCH bindings) runs over a read-only txn view under
/// a read lock; Phase 2 (the CREATE writes) records deltas with `Some(txn_id)`
/// under a write lock. Same two-lock discipline.
fn execute_unwind_mutation_in_txn(
    shared: &Arc<RwLock<Graph>>,
    txn_id: u64,
    mutation: &gql::MutationStatement,
    deadline: Option<Instant>,
) -> Result<GqlMutationResult, String> {
    use ermya_graph::gql::MutationClause;

    let unwind = mutation.unwind_clause.as_ref().ok_or_else(|| {
        "execute_unwind_mutation_in_txn invoked without an UNWIND clause".to_owned()
    })?;

    // UNWIND supports CREATE and DELETE; reject other clauses early.
    match &mutation.mutation {
        MutationClause::Create(_) | MutationClause::Delete(_) => {}
        other => {
            return Err(format!(
                "mutation clause not yet supported with UNWIND: {other:?}"
            ));
        }
    }

    // Phase 1 — read under a read lock over the txn snapshot view.
    let (elements, rows) = {
        let graph = shared
            .read()
            .map_err(|_| "graph lock poisoned".to_owned())?;
        let view = gql::TxnReadView::new(&graph, txn_id);
        gql::eval_unwind_and_match(&view, mutation, unwind, deadline)
            .map_err(engine_err_to_string)?
    };

    if elements.is_empty() || rows.is_empty() {
        return Ok(GqlMutationResult::default());
    }

    // Phase 2 — write under a write lock, scoped to this transaction.
    let mut graph = shared
        .write()
        .map_err(|_| "graph lock poisoned".to_owned())?;
    match &mutation.mutation {
        MutationClause::Create(create) => gql::apply_unwind_create_body(
            &mut graph,
            unwind,
            create,
            &elements,
            &rows,
            Some(txn_id),
        )
        .map_err(engine_err_to_string),
        MutationClause::Delete(dc) => {
            gql::apply_unwind_delete_body(&mut graph, &rows, dc, Some(txn_id))
                .map_err(engine_err_to_string)
        }
        other => Err(format!(
            "mutation clause not yet supported with UNWIND: {other:?}"
        )),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    use ermya_graph::gql::{self, GqlStatement, GqlValue};
    use ermya_graph::{Graph, Properties, Property};

    use super::{DefaultGraphAccessor, GraphAccessor};

    fn make_accessor() -> DefaultGraphAccessor {
        let graph = Arc::new(RwLock::new(Graph::new()));
        DefaultGraphAccessor::new(graph)
    }

    fn make_mvcc_accessor() -> DefaultGraphAccessor {
        let mut graph = Graph::new();
        graph.enable_mvcc();
        DefaultGraphAccessor::new(Arc::new(RwLock::new(graph)))
    }

    #[test]
    fn default_accessor_begin_commit_rollback_delegate_to_graph() {
        let accessor = make_mvcc_accessor();
        let txn = accessor.begin_txn().unwrap();
        accessor.commit_txn(txn).unwrap();

        let txn2 = accessor.begin_txn().unwrap();
        accessor.rollback_txn(txn2).unwrap();

        // Committing an already-closed transaction surfaces the engine error.
        assert!(accessor.commit_txn(txn).is_err());
    }

    fn parse_mutation(input: &str) -> gql::MutationStatement {
        match gql::parse_statement(input).expect("parse failed") {
            GqlStatement::Mutation(m) => m,
            GqlStatement::Query(_) => panic!("expected mutation, got query"),
            GqlStatement::Pipeline(_) => panic!("expected mutation, got pipeline"),
            GqlStatement::Admin(_) => panic!("expected mutation, got admin"),
            GqlStatement::ConstReturn(_) => {
                panic!("expected mutation, got const return")
            }
            GqlStatement::Ddl(_) => panic!("expected mutation, got ddl"),
            GqlStatement::Call(_) => panic!("expected mutation, got call"),
        }
    }

    fn add_person(accessor: &DefaultGraphAccessor, name: &str) {
        let props: Properties = [("name".to_owned(), Property::String(name.to_owned()))]
            .into_iter()
            .collect();
        accessor
            .graph
            .write()
            .unwrap()
            .add_node("Person", props)
            .unwrap();
    }

    #[test]
    fn parameterized_match_where_set_only_updates_the_matched_node() {
        let accessor = make_accessor();
        add_person(&accessor, "Alice");
        add_person(&accessor, "Bob");
        let mut statement = GqlStatement::Mutation(parse_mutation(
            "MATCH (n:Person) WHERE n.name = $name SET n.status = 'active'",
        ));
        let params = HashMap::from([("name".to_owned(), GqlValue::Str("Alice".to_owned()))]);
        gql::param_substitution::apply(&mut statement, &params).unwrap();
        let GqlStatement::Mutation(mutation) = statement else {
            unreachable!("constructed a mutation statement");
        };
        let alice_id = find_person(&accessor, "Alice");
        let bob_id = find_person(&accessor, "Bob");

        let (_rows, stats) = accessor.execute_mutation(&mutation, params, None).unwrap();
        assert_eq!(stats.properties_set, 1);

        let graph = accessor.graph.read().unwrap();
        let alice = graph.node(alice_id).unwrap();
        let bob = graph.node(bob_id).unwrap();
        assert_eq!(
            alice.properties().get("status"),
            Some(&Property::String("active".into()))
        );
        assert!(!bob.properties().contains_key("status"));
    }

    #[test]
    fn parameterized_match_where_set_only_updates_the_matched_node_in_a_transaction() {
        let accessor = make_mvcc_accessor();
        add_person(&accessor, "Alice");
        add_person(&accessor, "Bob");
        let mut statement = GqlStatement::Mutation(parse_mutation(
            "MATCH (n:Person) WHERE n.name = $name SET n.status = 'active'",
        ));
        let params = HashMap::from([("name".to_owned(), GqlValue::Str("Alice".to_owned()))]);
        gql::param_substitution::apply(&mut statement, &params).unwrap();
        let GqlStatement::Mutation(mutation) = statement else {
            unreachable!("constructed a mutation statement");
        };
        let alice_id = find_person(&accessor, "Alice");
        let bob_id = find_person(&accessor, "Bob");
        let txn_id = accessor.begin_txn().unwrap();

        let (_rows, stats) = accessor
            .execute_mutation_in_txn(txn_id, &mutation, params, None)
            .unwrap();
        assert_eq!(stats.properties_set, 1);
        accessor.commit_txn(txn_id).unwrap();

        let graph = accessor.graph.read().unwrap();
        let alice = graph.node(alice_id).unwrap();
        let bob = graph.node(bob_id).unwrap();
        assert_eq!(
            alice.properties().get("status"),
            Some(&Property::String("active".into()))
        );
        assert!(!bob.properties().contains_key("status"));
    }

    /// Returns the `NodeId` of the Person node whose `name` property equals `name`.
    fn find_person(accessor: &DefaultGraphAccessor, name: &str) -> ermya_graph::NodeId {
        let graph = accessor.graph.read().unwrap();
        let ids = graph.nodes_by_label("Person");
        for id in ids {
            if let Ok(node) = graph.node(id)
                && node
                    .properties()
                    .get("name")
                    .is_some_and(|v| v == &Property::String(name.to_owned()))
            {
                return id;
            }
        }
        panic!("Person with name='{name}' not found");
    }

    // ── Cycle 1 ───────────────────────────────────────────────────────────────

    /// Bare CREATE (a)-[:KNOWS]->(b) without MATCH must return an error
    /// explaining that a MATCH clause is required to bind the variables.
    #[test]
    fn bare_create_edge_is_rejected() {
        let accessor = make_accessor();
        let mutation = parse_mutation("CREATE (a:Person)-[:KNOWS]->(b:Person)");
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert!(result.is_err(), "expected Err, got {result:?}");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("MATCH clause"),
            "unexpected error message: {msg}"
        );
    }

    // ── Cycle 2 ───────────────────────────────────────────────────────────────

    /// MATCH…CREATE creates an edge between the matched nodes and returns (0, 1).
    #[test]
    fn match_create_edge_basic() {
        let accessor = make_accessor();
        add_person(&accessor, "Alice");
        add_person(&accessor, "Bob");

        let mutation = parse_mutation(
            "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) \
             CREATE (a)-[:KNOWS]->(b)",
        );
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((0, 1)),
            "expected (0, 1), got {result:?}"
        );

        // Verify the edge exists by querying outgoing edges from Alice.
        let alice_id = find_person(&accessor, "Alice");
        let graph = accessor.graph.read().unwrap();
        let out_edges = graph
            .outgoing_edges(alice_id)
            .expect("outgoing_edges failed");
        assert_eq!(out_edges.len(), 1);
        assert_eq!(out_edges[0].label(), "KNOWS");
    }

    // ── Cycle 3 ───────────────────────────────────────────────────────────────

    /// Edge properties in MATCH…CREATE are persisted on the created edge.
    #[test]
    fn match_create_edge_with_properties() {
        let accessor = make_accessor();
        add_person(&accessor, "Alice");
        add_person(&accessor, "Bob");

        let mutation = parse_mutation(
            "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) \
             CREATE (a)-[:KNOWS {since: 2024}]->(b)",
        );
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((0, 1))
        );

        let alice_id = find_person(&accessor, "Alice");
        let graph = accessor.graph.read().unwrap();
        let edges = graph
            .outgoing_edges(alice_id)
            .expect("outgoing_edges failed");
        assert_eq!(edges.len(), 1);
        let since = edges[0].properties().get("since").cloned();
        assert_eq!(
            since,
            Some(ermya_graph::Property::I64(2024)),
            "expected since=2024, got {since:?}"
        );
    }

    // ── Cycle 4 ───────────────────────────────────────────────────────────────

    /// When the MATCH clause finds no nodes, no edges are created and the
    /// result is Ok((0, 0)).
    #[test]
    fn match_create_edge_no_matches() {
        let accessor = make_accessor();
        // Graph is empty — no Person named 'Ghost' exists.
        let mutation = parse_mutation(
            "MATCH (a:Person {name: 'Ghost'}), (b:Person {name: 'Bob'}) \
             CREATE (a)-[:KNOWS]->(b)",
        );
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((0, 0))
        );
    }

    // ── Cycle 5 ───────────────────────────────────────────────────────────────

    /// A cross-join MATCH (1×3 = 3 rows) creates one edge per row.
    #[test]
    fn match_create_edge_multiple_matches() {
        let accessor = make_accessor();
        add_person(&accessor, "Alice");
        add_person(&accessor, "Bob");
        add_person(&accessor, "Carol");
        add_person(&accessor, "Dave");

        // (a:Person {name:'Alice'}) × (b:Person) = 1×4 = 4 rows
        // (Alice→Alice, Alice→Bob, Alice→Carol, Alice→Dave)
        let mutation = parse_mutation(
            "MATCH (a:Person {name: 'Alice'}), (b:Person) \
             CREATE (a)-[:KNOWS]->(b)",
        );
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((0, 4)),
            "expected (0, 4), got {result:?}"
        );

        // Verify outgoing edge count from Alice.
        let alice_id = find_person(&accessor, "Alice");
        let graph = accessor.graph.read().unwrap();
        let out_edges = graph
            .outgoing_edges(alice_id)
            .expect("outgoing_edges failed");
        assert_eq!(out_edges.len(), 4);
    }

    // ── Cycle 7 ───────────────────────────────────────────────────────────────

    /// MATCH…CREATE works correctly inside a batch (deferred WAL sync).
    #[test]
    fn match_create_edge_with_batch() {
        let accessor = make_accessor();
        add_person(&accessor, "Alice");
        add_person(&accessor, "Bob");

        accessor.begin_batch().expect("begin_batch failed");

        let mutation = parse_mutation(
            "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) \
             CREATE (a)-[:KNOWS]->(b)",
        );
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((0, 1))
        );

        accessor.end_batch().expect("end_batch failed");

        let alice_id = find_person(&accessor, "Alice");
        let graph = accessor.graph.read().unwrap();
        let edges = graph
            .outgoing_edges(alice_id)
            .expect("outgoing_edges failed");
        assert_eq!(edges.len(), 1);
    }

    // ── Cycle 6 regression: bare CREATE node still works ─────────────────────

    /// Bare CREATE node (no MATCH) continues to work correctly.
    #[test]
    fn bare_create_node_regression() {
        let accessor = make_accessor();
        let mutation = parse_mutation("CREATE (n:Person {name: 'Alice'})");
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((1, 0))
        );
    }

    // ── UNWIND+CREATE ─────────────────────────────────────────────────────────

    /// UNWIND [10, 20, 30] AS x MATCH (r:Root) CREATE (n:Item {val: x})
    /// creates 3 Item nodes with val=10, val=20, val=30.
    #[test]
    fn unwind_match_create_nodes() {
        let accessor = make_accessor();
        {
            let mut g = accessor.graph.write().unwrap();
            g.add_node("Root", Properties::default()).unwrap();
        }

        let mutation =
            parse_mutation("UNWIND [10, 20, 30] AS x MATCH (r:Root) CREATE (n:Item {val: x})");
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((3, 0)),
            "expected 3 nodes created, got {result:?}"
        );

        // Verify node properties.
        let graph = accessor.graph.read().unwrap();
        let item_ids = graph.nodes_by_label("Item");
        assert_eq!(item_ids.len(), 3);

        let mut vals: Vec<i64> = item_ids
            .iter()
            .map(|id| {
                let node = graph.node(*id).unwrap();
                match node.properties().get("val").unwrap() {
                    Property::I64(v) => *v,
                    other => panic!("expected I64, got {other:?}"),
                }
            })
            .collect();
        vals.sort_unstable();
        assert_eq!(vals, vec![10, 20, 30]);
    }

    /// UNWIND [] AS x MATCH (r:Root) CREATE (n:Item {val: x}) creates 0 nodes.
    #[test]
    fn unwind_empty_list_no_mutations() {
        let accessor = make_accessor();
        {
            let mut g = accessor.graph.write().unwrap();
            g.add_node("Root", Properties::default()).unwrap();
        }

        let mutation = parse_mutation("UNWIND [] AS x MATCH (r:Root) CREATE (n:Item {val: x})");
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((0, 0))
        );
    }

    /// UNWIND [1, 2, 3] AS x MATCH (r:Root) CREATE (n:Item {val: x + 10})
    /// creates 3 nodes with val=11, val=12, val=13.
    #[test]
    fn unwind_create_with_expression_prop() {
        let accessor = make_accessor();
        {
            let mut g = accessor.graph.write().unwrap();
            g.add_node("Root", Properties::default()).unwrap();
        }

        let mutation =
            parse_mutation("UNWIND [1, 2, 3] AS x MATCH (r:Root) CREATE (n:Item {val: x + 10})");
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((3, 0))
        );

        let graph = accessor.graph.read().unwrap();
        let item_ids = graph.nodes_by_label("Item");
        assert_eq!(item_ids.len(), 3);

        let mut vals: Vec<i64> = item_ids
            .iter()
            .map(|id| {
                let node = graph.node(*id).unwrap();
                match node.properties().get("val").unwrap() {
                    Property::I64(v) => *v,
                    other => panic!("expected I64, got {other:?}"),
                }
            })
            .collect();
        vals.sort_unstable();
        assert_eq!(vals, vec![11, 12, 13]);
    }

    /// UNWIND without MATCH creates nodes from list elements.
    #[test]
    fn unwind_create_without_match() {
        let accessor = make_accessor();

        let mutation = parse_mutation("UNWIND [100, 200] AS x CREATE (n:Item {val: x})");
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((2, 0))
        );

        let graph = accessor.graph.read().unwrap();
        let item_ids = graph.nodes_by_label("Item");
        assert_eq!(item_ids.len(), 2);

        let mut vals: Vec<i64> = item_ids
            .iter()
            .map(|id| {
                let node = graph.node(*id).unwrap();
                match node.properties().get("val").unwrap() {
                    Property::I64(v) => *v,
                    other => panic!("expected I64, got {other:?}"),
                }
            })
            .collect();
        vals.sort_unstable();
        assert_eq!(vals, vec![100, 200]);
    }

    /// Issue #15: `UNWIND range(a, b) AS i CREATE (...)` must persist `b-a+1`
    /// nodes over the mutation path, exactly like the literal-list form does.
    /// Before the fix, `range()` resolved only in the pipeline binding
    /// evaluator, so the server's `execute_unwind_mutation` (which evaluates
    /// the UNWIND source via `execute_expr`) saw `Null` and created 0 nodes.
    #[test]
    fn unwind_range_create_persists_nodes() {
        let accessor = make_accessor();

        let mutation = parse_mutation("UNWIND range(1, 3) AS i CREATE (n:M {i: i})");
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((3, 0)),
            "range(1,3) CREATE must persist 3 nodes, got {result:?}"
        );

        let graph = accessor.graph.read().unwrap();
        let item_ids = graph.nodes_by_label("M");
        assert_eq!(item_ids.len(), 3);

        let mut vals: Vec<i64> = item_ids
            .iter()
            .map(|id| {
                let node = graph.node(*id).unwrap();
                match node.properties().get("i").unwrap() {
                    Property::I64(v) => *v,
                    other => panic!("expected I64, got {other:?}"),
                }
            })
            .collect();
        vals.sort_unstable();
        assert_eq!(vals, vec![1, 2, 3]);
    }

    /// UNWIND+MATCH+CREATE with edge creation: creates nodes and edges
    /// linking back to the matched root.
    #[test]
    fn unwind_match_create_nodes_and_edges() {
        let accessor = make_accessor();
        {
            let mut g = accessor.graph.write().unwrap();
            g.add_node(
                "Root",
                [("name".to_owned(), Property::String("R".to_owned()))]
                    .into_iter()
                    .collect(),
            )
            .unwrap();
        }

        let mutation = parse_mutation(
            "UNWIND [1, 2] AS x MATCH (r:Root) CREATE (n:Item {val: x}), (r)-[:HAS]->(n)",
        );
        let result = accessor.execute_mutation(&mutation, HashMap::new(), None);
        assert_eq!(
            result
                .clone()
                .map(|(_r, s)| (s.nodes_created, s.edges_created)),
            Ok((2, 2)),
            "expected 2 nodes + 2 edges, got {result:?}"
        );

        // Verify edges from Root.
        let graph = accessor.graph.read().unwrap();
        let root_ids = graph.nodes_by_label("Root");
        assert_eq!(root_ids.len(), 1);
        let root_id = root_ids[0];
        let edges = graph.outgoing_edges(root_id).unwrap();
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| e.label() == "HAS"));
    }

    // ── Task 4 C4: Cap B at the GraphAccessor boundary + sentinel remap ──────

    #[test]
    fn cap_b_aborts_on_output_rows_over_limit() {
        let acc = make_accessor();
        for i in 0..10 {
            add_person(&acc, &format!("p{i}"));
        }
        let q = gql::parse("MATCH (a:Person) RETURN a").unwrap();
        let err = acc
            .execute_query(&q, HashMap::new(), 5, None)
            .expect_err("10 rows > cap 5 must abort");
        assert!(
            err.starts_with(super::ENGINE_RESULT_CAPPED_PREFIX),
            "Cap B error must carry the wire sentinel, got: {err}"
        );
    }

    #[test]
    fn cap_b_disabled_with_zero() {
        let acc = make_accessor();
        for i in 0..10 {
            add_person(&acc, &format!("p{i}"));
        }
        let q = gql::parse("MATCH (a:Person) RETURN a").unwrap();
        let rows = acc.execute_query(&q, HashMap::new(), 0, None).unwrap();
        assert_eq!(rows.len(), 10);
    }

    #[test]
    fn engine_err_to_string_remaps_result_cap_marker() {
        let e = ermya_graph::Error::GqlCompileError(format!(
            "{}query produced 99 rows",
            ermya_graph::gql::RESULT_CAP_MSG_PREFIX
        ));
        let s = super::engine_err_to_string(e);
        assert!(
            s.starts_with(super::ENGINE_RESULT_CAPPED_PREFIX),
            "result-cap GqlCompileError must be remapped, got: {s}"
        );
    }

    /// Issue #43: a write refused because the label is append-only must reach
    /// the Bolt handler tagged, and must keep the label name so the client is
    /// told which label refused the write.
    #[test]
    fn engine_err_to_string_remaps_append_only_in_txn_marker() {
        let e = ermya_graph::Error::AppendOnlyLabelInTxn {
            label: "Event".to_owned(),
        };
        let s = super::engine_err_to_string(e);
        assert!(
            s.starts_with(super::ENGINE_APPEND_ONLY_IN_TXN_PREFIX),
            "append-only rejection must be remapped, got: {s}"
        );
        assert!(
            s.contains("Event"),
            "the offending label must survive, got: {s}"
        );
    }

    /// Cycle A10: deleting a node that still has relationships (without
    /// DETACH) is a graph-integrity violation, and must reach the Bolt handler
    /// tagged so it can be answered with the dedicated schema code its
    /// docstring has always promised.
    #[test]
    fn engine_err_to_string_remaps_delete_connected_node_marker() {
        let e = ermya_graph::Error::DeleteConnectedNode {
            node: ermya_graph::NodeId::from_raw(7),
            relationships: 3,
        };
        let s = super::engine_err_to_string(e);
        assert!(
            s.starts_with(super::ENGINE_DELETE_CONNECTED_PREFIX),
            "connected-node delete must be remapped, got: {s}"
        );
        assert!(
            s.contains('3'),
            "the relationship count must survive, got: {s}"
        );
    }

    /// Cycle A11: the transaction memory cap must reach the handler tagged so
    /// it can be reported as transient (retryable) rather than as a flat
    /// execution failure.
    #[test]
    fn engine_err_to_string_remaps_txn_memory_cap_marker() {
        let e = ermya_graph::Error::TxnMemoryCapExceeded {
            txn_id: 4,
            used_bytes: 2048,
            cap_bytes: 1024,
        };
        let s = super::engine_err_to_string(e);
        assert!(
            s.starts_with(super::ENGINE_TXN_MEMORY_CAP_PREFIX),
            "txn memory cap must be remapped, got: {s}"
        );
    }

    /// Cycle A12: the batch cap must reach the handler tagged so it can be
    /// reported as an invalid request rather than something worth retrying.
    #[test]
    fn engine_err_to_string_remaps_batch_limit_marker() {
        let e = ermya_graph::Error::BatchLimitExceeded {
            kind: ermya_graph::BatchLimitKind::Operations,
            current: 5000,
            limit: 1000,
        };
        let s = super::engine_err_to_string(e);
        assert!(
            s.starts_with(super::ENGINE_BATCH_LIMIT_PREFIX),
            "batch limit must be remapped, got: {s}"
        );
    }

    #[test]
    fn engine_err_to_string_leaves_ordinary_compile_error() {
        let e = ermya_graph::Error::GqlCompileError("ordinary scope error".to_owned());
        let s = super::engine_err_to_string(e);
        assert!(!s.starts_with(super::ENGINE_RESULT_CAPPED_PREFIX));
        assert!(!s.starts_with(super::ENGINE_QUOTA_EXCEEDED_PREFIX));
        assert!(!s.starts_with(super::ENGINE_QUERY_TIMEOUT_PREFIX));
    }

    // ── Task 6: query-timeout sentinel remap at the GraphAccessor boundary ──

    #[test]
    fn engine_err_to_string_remaps_query_timeout_marker() {
        // The engine aborts a runaway query with a GqlCompileError carrying
        // TIMEOUT_MSG_PREFIX; the boundary must rewrite it to the server-side
        // ENGINE_QUERY_TIMEOUT_PREFIX so the handler surfaces the non-retryable
        // wire code. Deterministic — no clock involved.
        let e = ermya_graph::Error::GqlCompileError(format!(
            "{}query exceeded time budget",
            ermya_graph::gql::TIMEOUT_MSG_PREFIX
        ));
        let s = super::engine_err_to_string(e);
        assert!(
            s.starts_with(super::ENGINE_QUERY_TIMEOUT_PREFIX),
            "timeout GqlCompileError must be remapped, got: {s}"
        );
    }

    #[test]
    fn engine_err_to_string_does_not_confuse_timeout_with_result_cap() {
        // The two sentinels share the GqlCompileError carrier; a timeout abort
        // must NOT be misclassified as a result-cap abort (different wire code:
        // ExecutionFailed vs ResultExhausted).
        let e = ermya_graph::Error::GqlCompileError(format!(
            "{}query exceeded time budget",
            ermya_graph::gql::TIMEOUT_MSG_PREFIX
        ));
        let s = super::engine_err_to_string(e);
        assert!(
            !s.starts_with(super::ENGINE_RESULT_CAPPED_PREFIX),
            "timeout abort must not be remapped as a result-cap abort, got: {s}"
        );
    }

    // ── Cycle 5.3: MERGE executor (probes A, B, D, E) ───────────────────────────

    /// MERGE creates a node when no match exists, and is idempotent on a
    /// second identical MERGE (probe A — basic MERGE).
    #[test]
    fn merge_creates_node_when_not_found() {
        let accessor = make_accessor();
        let mutation = parse_mutation("MERGE (n:AssetNode {id: 'x'})");
        let (rows, stats) = accessor
            .execute_mutation(&mutation, HashMap::new(), None)
            .unwrap();
        assert_eq!(stats.nodes_created, 1, "expected 1 node created");
        assert!(rows.is_empty(), "no RETURN clause — rows must be empty");

        // Second MERGE must not create a second node.
        let mutation2 = parse_mutation("MERGE (n:AssetNode {id: 'x'})");
        let (rows2, stats2) = accessor
            .execute_mutation(&mutation2, HashMap::new(), None)
            .unwrap();
        assert_eq!(
            stats2.nodes_created, 0,
            "second MERGE must not create a duplicate"
        );
        assert!(rows2.is_empty());

        // Verify exactly one node exists.
        let graph = accessor.graph.read().unwrap();
        assert_eq!(graph.nodes_by_label("AssetNode").len(), 1);
    }

    /// MERGE with RETURN n returns the merged node as a single row (probe B).
    #[test]
    fn merge_with_return_returns_node_row() {
        let accessor = make_accessor();
        let mutation = parse_mutation("MERGE (n:AssetNode {id: 'x'}) RETURN n");
        let (rows, _stats) = accessor
            .execute_mutation(&mutation, HashMap::new(), None)
            .unwrap();
        assert_eq!(rows.len(), 1, "expected one result row");
        assert!(rows[0].contains_key("n"), "row must contain column 'n'");
    }

    /// MERGE ON CREATE SET applies properties when the node is created (probe D).
    #[test]
    fn merge_on_create_set_applies_on_create() {
        let accessor = make_accessor();
        let mutation = parse_mutation("MERGE (n:Person {id: 'p1'}) ON CREATE SET n.name = 'Alice'");
        accessor
            .execute_mutation(&mutation, HashMap::new(), None)
            .unwrap();

        let graph = accessor.graph.read().unwrap();
        let ids = graph.nodes_by_label("Person");
        assert_eq!(ids.len(), 1);
        let node = graph.node(ids[0]).unwrap();
        assert_eq!(
            node.properties().get("name"),
            Some(&Property::String("Alice".into())),
        );
    }

    /// MERGE ON MATCH SET applies properties when the node already exists
    /// (probe E partial — the ON MATCH branch).
    #[test]
    fn merge_on_match_set_applies_on_second_merge() {
        let accessor = make_accessor();
        // Create the node via first MERGE.
        let m1 = parse_mutation("MERGE (n:Person {id: 'p1'}) ON CREATE SET n.name = 'Alice'");
        accessor
            .execute_mutation(&m1, HashMap::new(), None)
            .unwrap();

        // Second MERGE must apply ON MATCH SET, not create a duplicate.
        let m2 = parse_mutation("MERGE (n:Person {id: 'p1'}) ON MATCH SET n.name = 'AliceUpdated'");
        accessor
            .execute_mutation(&m2, HashMap::new(), None)
            .unwrap();

        let graph = accessor.graph.read().unwrap();
        let ids = graph.nodes_by_label("Person");
        assert_eq!(ids.len(), 1, "still only one node");
        let node = graph.node(ids[0]).unwrap();
        assert_eq!(
            node.properties().get("name"),
            Some(&Property::String("AliceUpdated".into())),
        );
    }

    // ── Cycle 5.4 — MATCH … SET n = $map / SET n += $map (probes F, G) ─────────

    /// `MATCH (n:Person {name: 'Alice'}) SET n = $props` overwrites every
    /// property from the map: keys absent from the map are dropped.
    #[test]
    fn match_set_entity_overwrite_from_map() {
        let accessor = make_accessor();
        // Seed with two properties; one ("age") must disappear after overwrite.
        {
            let props: Properties = [
                ("name".to_owned(), Property::String("Alice".to_owned())),
                ("age".to_owned(), Property::I64(30)),
            ]
            .into_iter()
            .collect();
            accessor
                .graph
                .write()
                .unwrap()
                .add_node("Person", props)
                .unwrap();
        }

        let mut stmt =
            gql::parse_statement("MATCH (n:Person {name: 'Alice'}) SET n = $props").unwrap();
        let params = HashMap::from([(
            "props".to_owned(),
            gql::GqlValue::Map(HashMap::from([
                ("name".to_owned(), gql::GqlValue::Str("Alice2".into())),
                ("score".to_owned(), gql::GqlValue::Int(100)),
            ])),
        )]);
        gql::param_substitution::apply(&mut stmt, &params).unwrap();
        let mutation = match stmt {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };

        // The handler threads the SAME param map (Maps survive substitution as
        // bare ParamRefs) into execute_mutation; mirror that contract here.
        accessor.execute_mutation(&mutation, params, None).unwrap();

        let id = find_person(&accessor, "Alice2");
        let graph = accessor.graph.read().unwrap();
        let node = graph.node(id).unwrap();
        assert_eq!(node.properties().get("score"), Some(&Property::I64(100)));
        assert_eq!(
            node.properties().get("name"),
            Some(&Property::String("Alice2".into())),
        );
        // "age" was not in $props → overwrite must have removed it.
        assert_eq!(
            node.properties().get("age"),
            None,
            "overwrite must drop unset keys"
        );
    }

    /// `MATCH (n:Person {name: 'Alice'}) SET n += $props` merges the map:
    /// existing keys absent from the map are preserved.
    #[test]
    fn match_set_entity_merge_from_map() {
        let accessor = make_accessor();
        add_person(&accessor, "Alice");

        let mut stmt =
            gql::parse_statement("MATCH (n:Person {name: 'Alice'}) SET n += $props").unwrap();
        let params = HashMap::from([(
            "props".to_owned(),
            gql::GqlValue::Map(HashMap::from([(
                "score".to_owned(),
                gql::GqlValue::Int(99),
            )])),
        )]);
        gql::param_substitution::apply(&mut stmt, &params).unwrap();
        let mutation = match stmt {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };

        accessor.execute_mutation(&mutation, params, None).unwrap();

        let id = find_person(&accessor, "Alice");
        let graph = accessor.graph.read().unwrap();
        let node = graph.node(id).unwrap();
        // "name" preserved (merge), "score" added.
        assert_eq!(
            node.properties().get("name"),
            Some(&Property::String("Alice".into())),
        );
        assert_eq!(node.properties().get("score"), Some(&Property::I64(99)));
    }

    /// `MERGE (n:Person {id:'p1'}) ON CREATE SET n = $props` resolves a `$map`
    /// param through the MERGE executor (probe E, ON CREATE branch). Regression
    /// guard for the wiring fix that threads `params` into `execute_bare_merge`
    /// — before it, the bare `ParamRef` that survives substitution panicked in
    /// `execute_expr`.
    #[test]
    fn merge_on_create_set_entity_overwrite_from_map() {
        let accessor = make_accessor();

        let mut stmt =
            gql::parse_statement("MERGE (n:Person {id: 'p1'}) ON CREATE SET n = $props").unwrap();
        let params = HashMap::from([(
            "props".to_owned(),
            gql::GqlValue::Map(HashMap::from([
                ("name".to_owned(), gql::GqlValue::Str("Alice".into())),
                ("score".to_owned(), gql::GqlValue::Int(7)),
            ])),
        )]);
        gql::param_substitution::apply(&mut stmt, &params).unwrap();
        let mutation = match stmt {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };

        accessor.execute_mutation(&mutation, params, None).unwrap();

        let graph = accessor.graph.read().unwrap();
        let ids = graph.nodes_by_label("Person");
        assert_eq!(ids.len(), 1, "exactly one node created");
        let node = graph.node(ids[0]).unwrap();
        // ON CREATE SET n = $props applied the whole map on top of the merge key.
        assert_eq!(
            node.properties().get("name"),
            Some(&Property::String("Alice".into()))
        );
        assert_eq!(node.properties().get("score"), Some(&Property::I64(7)));
    }

    /// Issue #26 (real .NET probe variant G): `SET n += $map` must count each
    /// entry of the merged map as a property set, so the driver's
    /// `PropertiesSet` counter is correct.
    #[test]
    fn set_entity_merge_from_map_counts_properties_set() {
        let accessor = make_accessor();
        // Seed a node to match.
        {
            let mut g = accessor.graph.write().unwrap();
            g.add_node("AssetNode", ermya_graph::props! { "id" => "x" })
                .unwrap();
        }

        let mut stmt =
            gql::parse_statement("MATCH (n:AssetNode {id: 'x'}) SET n += $props").unwrap();
        let params = HashMap::from([(
            "props".to_owned(),
            gql::GqlValue::Map(HashMap::from([
                ("name".to_owned(), gql::GqlValue::Str("Asset".into())),
                ("status".to_owned(), gql::GqlValue::Str("Active".into())),
            ])),
        )]);
        gql::param_substitution::apply(&mut stmt, &params).unwrap();
        let mutation = match stmt {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };

        let (_rows, stats) = accessor.execute_mutation(&mutation, params, None).unwrap();
        assert_eq!(
            stats.properties_set, 2,
            "two map entries merged onto the node"
        );
    }

    /// Issue #26 (real .NET probe variant H): `CREATE (n:L $map)` must count each
    /// entry of the inline map as a property set.
    #[test]
    fn create_inline_map_counts_properties_set() {
        let accessor = make_accessor();

        let mut stmt = gql::parse_statement("CREATE (n:Template $props) RETURN n").unwrap();
        let params = HashMap::from([(
            "props".to_owned(),
            gql::GqlValue::Map(HashMap::from([
                ("k".to_owned(), gql::GqlValue::Str("v".into())),
                ("k2".to_owned(), gql::GqlValue::Int(9)),
            ])),
        )]);
        gql::param_substitution::apply(&mut stmt, &params).unwrap();
        let mutation = match stmt {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };

        let (_rows, stats) = accessor.execute_mutation(&mutation, params, None).unwrap();
        assert_eq!(stats.nodes_created, 1);
        assert_eq!(
            stats.properties_set, 2,
            "two inline-map properties on the created node"
        );
    }

    // ── Cycle 5.5 — CREATE (n:Label $map) prop_map expansion (probe H) ─────────

    /// `CREATE (n:Widget $props)` expands the `$map` param into the new node's
    /// properties. The Map param is NOT substituted into the AST (no
    /// `Literal::Map`); it survives as a `prop_map` `ParamRef` the executor
    /// reads from the runtime params map.
    #[test]
    fn bare_create_with_map_param_expands_properties() {
        let accessor = make_accessor();

        let mut stmt = gql::parse_statement("CREATE (n:Widget $props)").unwrap();
        let params = HashMap::from([(
            "props".to_owned(),
            gql::GqlValue::Map(HashMap::from([
                ("color".to_owned(), gql::GqlValue::Str("blue".into())),
                ("weight".to_owned(), gql::GqlValue::Int(5)),
            ])),
        )]);
        // Mirror the handler: substitution runs (a no-op for the Map ParamRef),
        // then the SAME param map is threaded into execute_mutation.
        gql::param_substitution::apply(&mut stmt, &params).unwrap();
        let mutation = match stmt {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };

        accessor.execute_mutation(&mutation, params, None).unwrap();

        let graph = accessor.graph.read().unwrap();
        let ids = graph.nodes_by_label("Widget");
        assert_eq!(ids.len(), 1, "exactly one Widget created");
        let node = graph.node(ids[0]).unwrap();
        assert_eq!(
            node.properties().get("color"),
            Some(&Property::String("blue".into())),
        );
        assert_eq!(node.properties().get("weight"), Some(&Property::I64(5)));
    }

    // ── Cycle 5.6 — trailing RETURN after MATCH…SET and CREATE (probes F,G,H) ──
    //
    // These exercise the EXACT statements the .NET driver sends, parsed from
    // text (not hand-built AST), including the `RETURN n` the bare-form tests
    // of 5.4/5.5 omitted — which is what the e2e probe caught.

    /// Probe F: `MATCH (n) SET n = $map RETURN n` parses, overwrites, and
    /// projects the updated node as a single `{n: Map}` row.
    #[test]
    fn match_set_overwrite_with_trailing_return_projects_node() {
        let accessor = make_accessor();
        add_person(&accessor, "Alice");

        let mut stmt =
            gql::parse_statement("MATCH (n:Person {name: 'Alice'}) SET n = $properties RETURN n")
                .unwrap();
        let params = HashMap::from([(
            "properties".to_owned(),
            gql::GqlValue::Map(HashMap::from([
                ("name".to_owned(), gql::GqlValue::Str("X".into())),
                ("status".to_owned(), gql::GqlValue::Str("Active".into())),
            ])),
        )]);
        gql::param_substitution::apply(&mut stmt, &params).unwrap();
        let mutation = match stmt {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };

        let (rows, _stats) = accessor.execute_mutation(&mutation, params, None).unwrap();
        assert_eq!(rows.len(), 1, "RETURN n yields one row");
        let GqlValue::Map(projected) = &rows[0]["n"] else {
            panic!("n must project as a Map, got {:?}", rows[0]["n"]);
        };
        assert_eq!(
            projected.get("status"),
            Some(&GqlValue::Str("Active".into()))
        );
        assert_eq!(projected.get("name"), Some(&GqlValue::Str("X".into())));
    }

    /// Probe G: `MATCH (n) SET n += $map RETURN n` parses, merges, projects.
    #[test]
    fn match_set_merge_with_trailing_return_projects_node() {
        let accessor = make_accessor();
        add_person(&accessor, "Alice");

        let mut stmt =
            gql::parse_statement("MATCH (n:Person {name: 'Alice'}) SET n += $properties RETURN n")
                .unwrap();
        let params = HashMap::from([(
            "properties".to_owned(),
            gql::GqlValue::Map(HashMap::from([(
                "extra".to_owned(),
                gql::GqlValue::Str("Y".into()),
            )])),
        )]);
        gql::param_substitution::apply(&mut stmt, &params).unwrap();
        let mutation = match stmt {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };

        let (rows, _stats) = accessor.execute_mutation(&mutation, params, None).unwrap();
        assert_eq!(rows.len(), 1);
        let GqlValue::Map(projected) = &rows[0]["n"] else {
            panic!("n must project as a Map");
        };
        // merge preserved 'name', added 'extra'.
        assert_eq!(projected.get("name"), Some(&GqlValue::Str("Alice".into())));
        assert_eq!(projected.get("extra"), Some(&GqlValue::Str("Y".into())));
    }

    /// Probe H: `CREATE (n:Label $map) RETURN n` parses, expands, projects.
    #[test]
    fn bare_create_with_map_and_trailing_return_projects_node() {
        let accessor = make_accessor();

        let mut stmt = gql::parse_statement("CREATE (n:Template $properties) RETURN n").unwrap();
        let params = HashMap::from([(
            "properties".to_owned(),
            gql::GqlValue::Map(HashMap::from([(
                "k".to_owned(),
                gql::GqlValue::Str("v".into()),
            )])),
        )]);
        gql::param_substitution::apply(&mut stmt, &params).unwrap();
        let mutation = match stmt {
            GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };

        let (rows, stats) = accessor.execute_mutation(&mutation, params, None).unwrap();
        assert_eq!(stats.nodes_created, 1);
        assert_eq!(rows.len(), 1, "RETURN n yields one row");
        let GqlValue::Map(projected) = &rows[0]["n"] else {
            panic!("n must project as a Map");
        };
        assert_eq!(projected.get("k"), Some(&GqlValue::Str("v".into())));
    }

    // ── Phase 4: transactional accessor routes (`*_in_txn`) ──────────────────
    //
    // A CREATE run inside an open transaction writes a *pending* node, invisible
    // to auto-commit until COMMIT, yet visible to a MATCH issued inside the same
    // transaction (read-your-writes). The accessor delegates to the engine's
    // unified write path with `Some(txn_id)` and drives the lookup over a
    // read-only txn view under a read lock — the two-lock discipline the
    // contention measurement chose, preserved for the transactional path.

    #[test]
    fn execute_mutation_in_txn_writes_pending_invisible_to_autocommit() {
        let accessor = make_mvcc_accessor();
        let txn = accessor.begin_txn().unwrap();

        let mutation = parse_mutation("CREATE (n:Persona)");
        let (_rows, stats) = accessor
            .execute_mutation_in_txn(txn, &mutation, HashMap::new(), None)
            .unwrap();
        assert_eq!(
            stats.nodes_created, 1,
            "CREATE inside txn reports one node created"
        );

        // Auto-commit read must not see the pending node before COMMIT.
        let q = gql::parse("MATCH (n:Persona) RETURN n").unwrap();
        let autocommit_rows = accessor.execute_query(&q, HashMap::new(), 0, None).unwrap();
        assert_eq!(
            autocommit_rows.len(),
            0,
            "pending write invisible to auto-commit"
        );

        accessor.commit_txn(txn).unwrap();
        let after = accessor.execute_query(&q, HashMap::new(), 0, None).unwrap();
        assert_eq!(
            after.len(),
            1,
            "COMMIT makes the node visible to auto-commit"
        );
    }

    #[test]
    fn execute_query_in_txn_sees_own_uncommitted_write() {
        let accessor = make_mvcc_accessor();
        let txn = accessor.begin_txn().unwrap();

        let mutation = parse_mutation("CREATE (n:Persona)");
        accessor
            .execute_mutation_in_txn(txn, &mutation, HashMap::new(), None)
            .unwrap();

        // A MATCH issued *inside the same txn* sees the node it just created.
        let q = gql::parse("MATCH (n:Persona) RETURN n").unwrap();
        let in_txn_rows = accessor
            .execute_query_in_txn(txn, &q, HashMap::new(), 0, None)
            .unwrap();
        assert_eq!(in_txn_rows.len(), 1, "read-your-writes inside the txn");

        accessor.rollback_txn(txn).unwrap();
    }

    // ── Cycle 23: MERGE / UNWIND inside a transaction ────────────────────────

    /// A second MERGE on the same key inside the same txn must find the node the
    /// first MERGE created (not create a duplicate) — the lookup runs over the
    /// txn's own pending overlay, so this exercises read-your-writes on the
    /// MERGE lookup path specifically.
    #[test]
    fn merge_in_txn_second_call_matches_own_pending_node() {
        let accessor = make_mvcc_accessor();
        let txn = accessor.begin_txn().unwrap();

        let merge = parse_mutation("MERGE (n:Persona {id: 1})");
        let (_r1, s1) = accessor
            .execute_mutation_in_txn(txn, &merge, HashMap::new(), None)
            .unwrap();
        assert_eq!(s1.nodes_created, 1, "first MERGE creates the node");

        let (_r2, s2) = accessor
            .execute_mutation_in_txn(txn, &merge, HashMap::new(), None)
            .unwrap();
        assert_eq!(
            s2.nodes_created, 0,
            "second MERGE in the same txn matches its own pending node"
        );

        accessor.commit_txn(txn).unwrap();
        // Exactly one node after commit — no duplicate leaked.
        let q = gql::parse("MATCH (n:Persona) RETURN n").unwrap();
        let rows = accessor.execute_query(&q, HashMap::new(), 0, None).unwrap();
        assert_eq!(rows.len(), 1, "MERGE stayed idempotent inside the txn");
    }

    /// UNWIND … CREATE inside a txn writes N pending nodes, all visible to a
    /// MATCH issued inside the same txn (exercises the enumeration overlay via
    /// the transactional path), and invisible to auto-commit until COMMIT.
    #[test]
    fn unwind_create_in_txn_pending_nodes_visible_in_same_txn() {
        let accessor = make_mvcc_accessor();
        let txn = accessor.begin_txn().unwrap();

        let unwind = parse_mutation("UNWIND [1, 2, 3] AS x CREATE (n:Item {val: x})");
        let (_rows, stats) = accessor
            .execute_mutation_in_txn(txn, &unwind, HashMap::new(), None)
            .unwrap();
        assert_eq!(stats.nodes_created, 3, "UNWIND creates three pending nodes");

        let q = gql::parse("MATCH (n:Item) RETURN n").unwrap();
        let in_txn = accessor
            .execute_query_in_txn(txn, &q, HashMap::new(), 0, None)
            .unwrap();
        assert_eq!(in_txn.len(), 3, "all three pending nodes enumerated in-txn");

        let autocommit = accessor.execute_query(&q, HashMap::new(), 0, None).unwrap();
        assert_eq!(
            autocommit.len(),
            0,
            "pending nodes invisible to auto-commit"
        );

        accessor.commit_txn(txn).unwrap();
        let after = accessor.execute_query(&q, HashMap::new(), 0, None).unwrap();
        assert_eq!(after.len(), 3, "COMMIT publishes all three nodes");
    }

    /// MATCH…CREATE inside a txn: the MATCH sees a node the same txn created
    /// earlier, and the CREATE writes an edge pending in the same txn.
    #[test]
    fn match_create_edge_in_txn_over_own_pending_nodes() {
        let accessor = make_mvcc_accessor();
        let txn = accessor.begin_txn().unwrap();

        // Create two pending Person nodes inside the txn.
        for name in ["Alice", "Bob"] {
            let create = parse_mutation(&format!("CREATE (n:Person {{name: '{name}'}})"));
            accessor
                .execute_mutation_in_txn(txn, &create, HashMap::new(), None)
                .unwrap();
        }

        // MATCH both (own pending nodes) and CREATE an edge between them.
        let mc = parse_mutation(
            "MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) \
             CREATE (a)-[:KNOWS]->(b)",
        );
        let (_rows, stats) = accessor
            .execute_mutation_in_txn(txn, &mc, HashMap::new(), None)
            .unwrap();
        assert_eq!(
            (stats.nodes_created, stats.edges_created),
            (0, 1),
            "one edge created over own pending nodes"
        );

        accessor.commit_txn(txn).unwrap();
        // After commit the edge is durable.
        let q = gql::parse("MATCH (a:Person {name: 'Alice'})-[:KNOWS]->(b) RETURN b").unwrap();
        let rows = accessor.execute_query(&q, HashMap::new(), 0, None).unwrap();
        assert_eq!(rows.len(), 1, "committed edge is traversable");
    }
}
