// SPDX-License-Identifier: MIT

//! GQL abstract syntax tree types.

/// Top-level GQL read-only query.
#[derive(Debug, Clone, PartialEq)]
pub struct GqlQuery {
    /// Optional UNWIND clause (produces rows from a list expression).
    pub unwind_clause: Option<UnwindClause>,
    /// The mandatory MATCH clause.
    pub match_clause: MatchClause,
    /// The optional WHERE predicate.
    pub where_clause: Option<WhereClause>,
    /// The mandatory RETURN clause.
    pub return_clause: ReturnClause,
    /// The optional GROUP BY clause.
    pub group_by: Option<GroupByClause>,
    /// The optional ORDER BY clause.
    pub order_by: Option<OrderByClause>,
    /// The optional LIMIT clause.
    pub limit: Option<LimitClause>,
}

/// An UNWIND clause that produces one row per list element.
///
/// ```text
/// UNWIND [1, 2, 3] AS x
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    /// The list expression to unwind.
    pub expr: Expr,
    /// The variable binding each element.
    pub var: String,
}

/// A GROUP BY clause that groups rows by one or more key expressions.
///
/// ```text
/// GROUP BY p.dept, p.role
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct GroupByClause {
    /// The grouping key expressions.
    pub keys: Vec<Expr>,
}

/// One or more comma-separated path patterns following `MATCH`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    /// The list of path patterns to match.
    pub patterns: Vec<PathPattern>,
    /// Path variable bound by `MATCH p = (…)`. `None` when no binding.
    /// Consumed by the compiler's path projection (Fase B C6).
    pub path_var: Option<String>,
}

/// Filtering predicate following `WHERE`.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereClause {
    /// The boolean expression that must hold.
    pub predicate: Expr,
}

/// Projection specification following `RETURN`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    /// Whether duplicate rows should be removed.
    pub distinct: bool,
    /// The projected expressions with optional aliases.
    pub items: Vec<ReturnItem>,
}

/// A single projected expression in a RETURN clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    /// The expression to evaluate and project.
    pub expr: Expr,
    /// The optional `AS <alias>` name.
    pub alias: Option<String>,
}

/// Ordering specification following `ORDER BY`.
///
/// Cannot derive `Eq` because `OrderItem` transitively contains `Expr`,
/// which contains `Literal::Float(f64)`.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByClause {
    /// The ordered list of sort keys.
    pub items: Vec<OrderItem>,
}

/// A single sort key in an ORDER BY clause.
///
/// Cannot derive `Eq` because `Expr` transitively contains `Literal::Float(f64)`.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// The expression used as the sort key.
    pub expr: Expr,
    /// `true` for ASC (the default), `false` for DESC.
    pub ascending: bool,
}

/// Row-count restriction following `LIMIT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitClause {
    /// The maximum number of rows to return.
    pub count: u64,
}

/// A complete graph path pattern: `(a)-[r]->(b)-[s]->(c)`.
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// The leftmost node in the pattern.
    pub start: NodePattern,
    /// Each subsequent (edge, node) hop along the path.
    pub hops: Vec<(EdgePattern, NodePattern)>,
}

/// A node selector: `(var:Label {key: value})`.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    /// The optional binding variable name.
    pub var: Option<String>,
    /// The node labels to match.
    pub labels: Vec<String>,
    /// Inline property equality constraints.
    pub props: Vec<(String, Literal)>,
}

/// An edge selector: `-[var:LABEL {key: value}]->`.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgePattern {
    /// The optional binding variable name.
    pub var: Option<String>,
    /// The relationship types to match.
    pub labels: Vec<String>,
    /// Inline property equality constraints.
    pub props: Vec<(String, Literal)>,
    /// The direction of traversal.
    pub direction: AstDirection,
    /// Fixed hop or variable-length range.
    pub length: EdgeLength,
}

/// Traversal direction for an edge pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstDirection {
    /// `-[r]->`: left-to-right.
    Outgoing,
    /// `<-[r]-`: right-to-left.
    Incoming,
    /// `-[r]-`: undirected.
    Both,
}

/// Hop-count constraint on a variable-length edge pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeLength {
    /// Exactly one hop (the default).
    Fixed,
    /// A range of hops `-[*min..max]->`.
    Variable {
        /// Inclusive lower bound (`None` means 0).
        min: Option<u32>,
        /// Inclusive upper bound (`None` means unbounded).
        max: Option<u32>,
    },
}

/// All expression forms that can appear in WHERE, RETURN, and ORDER BY.
///
/// Cannot derive `Eq` because `Literal::Float(f64)` does not implement `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A constant value.
    Literal(Literal),
    /// A bound variable reference, e.g. `a`.
    Var(String),
    /// A property access, e.g. `a.name`.
    PropAccess {
        /// The variable holding the graph element.
        var: String,
        /// The property key to look up.
        prop: String,
    },
    /// A binary infix operation, e.g. `a.age > 30`.
    BinaryOp {
        /// Left-hand operand.
        left: Box<Self>,
        /// The operator.
        op: BinOp,
        /// Right-hand operand.
        right: Box<Self>,
    },
    /// A unary prefix operation, e.g. `NOT x`.
    UnaryOp {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<Self>,
    },
    /// An IS NULL / IS NOT NULL check.
    IsNull {
        /// The expression being tested.
        expr: Box<Self>,
        /// `true` when the surface syntax is `IS NOT NULL`.
        negated: bool,
    },
    /// An aggregation call, e.g. `COUNT(*)` or `SUM(a.salary)`.
    Aggregate {
        /// The aggregation function.
        func: AggFunc,
        /// The argument expression, or `None` for `COUNT(*)`.
        arg: Option<Box<Self>>,
    },
    /// A scalar function call, e.g. `id(n)`, `type(r)`, `labels(n)`.
    FunctionCall {
        /// The function name (lowercase).
        name: String,
        /// The argument expressions.
        args: Vec<Self>,
    },
    /// A list predicate, e.g. `ALL(x IN list WHERE x > 0)`.
    ///
    /// Binds `var` to each element of `list` in turn and tests `predicate`,
    /// applying the quantifier semantics of `kind`. The surface forms are
    /// `ALL`/`ANY`/`NONE`/`SINGLE` (case-insensitive).
    ListPredicate {
        /// The quantifier (`ALL`/`ANY`/`NONE`/`SINGLE`).
        kind: ListPredKind,
        /// The iteration variable bound to each list element.
        var: String,
        /// The list-valued expression being iterated.
        list: Box<Self>,
        /// The boolean predicate evaluated against each element.
        predicate: Box<Self>,
    },
    /// A `shortestPath((a)-[*..N]->(b))` Cypher-style expression.
    ShortestPath {
        /// The path pattern with exactly one hop (start → end).
        ///
        /// Boxed to keep `Expr` size down — `PathPattern` is significantly
        /// larger than other variants and an unboxed inline would bloat
        /// every `Expr` node, causing stack overflow on deep nesting.
        pattern: Box<PathPattern>,
    },
    /// A list subscript, e.g. `nodes[i]`.
    Subscript {
        /// The list-valued expression.
        list: Box<Self>,
        /// The zero-based integer index.
        index: Box<Self>,
    },
    /// An inline list literal, e.g. `[1, 2, 3]` (differs from `Literal::List`
    /// in that its elements are arbitrary expressions, not literals).
    ListLit(Vec<Self>),
    /// A parameter placeholder, resolved before compilation via the
    /// `param_substitution` module (added in cycle 6 of the parser fix).
    ///
    /// The compiler must never see this variant — that would indicate a
    /// programming error (substitution skipped between parse and compile).
    /// The compiler asserts on it defensively.
    ParamRef(ParamRef),
}

/// A parameter placeholder of the form `$name` or `$1`.
///
/// `Named` is resolved by string key against the params map.
/// `Positional(n)` is 1-based per the Bolt wire spec; the resolver looks it
/// up by the string key `"n"` because `PackStream` dicts use string keys
/// throughout.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ParamRef {
    /// `$name` — resolved by string key in the params map.
    Named(String),
    /// `$1`, `$2`, … — 1-based; resolved by looking up the string key
    /// `n.to_string()` in the params map (Bolt-spec convention).
    Positional(u32),
}

/// A literal value that can appear in expressions or inline property constraints.
///
/// Cannot derive `Eq` because `Float(f64)` does not implement `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A 64-bit signed integer.
    Int(i64),
    /// A 64-bit IEEE 754 float.
    Float(f64),
    /// A UTF-8 string.
    Str(String),
    /// A boolean.
    Bool(bool),
    /// The SQL/GQL NULL value.
    Null,
    /// A list literal, e.g. `[1, 2, 3]`, used for `IN` predicates.
    List(Vec<Self>),
}

/// Binary infix operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `=`
    Eq,
    /// `<>`
    NotEq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    LtEq,
    /// `>=`
    GtEq,
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `STARTS WITH` — Cypher string prefix predicate.
    StartsWith,
    /// `ENDS WITH` — Cypher string suffix predicate.
    EndsWith,
    /// `CONTAINS` — Cypher substring predicate.
    Contains,
    /// `IN` — Cypher list membership predicate.
    In,
}

/// Unary prefix operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `NOT`
    Not,
    /// Arithmetic negation `-`.
    Neg,
}

/// The unified top-level GQL statement.
#[derive(Debug, Clone, PartialEq)]
pub enum GqlStatement {
    /// A read-only query.
    Query(GqlQuery),
    /// A write mutation.
    Mutation(MutationStatement),
    /// A multi-stage pipeline (uses `WITH`).
    Pipeline(PipelineQuery),
    /// An admin statement (user management). Parsed by
    /// `tessera-graph-cypher` and executed by the server's admin handler;
    /// the core engine has no concept of users.
    Admin(AdminStatement),
    /// A `RETURN <expr-list>` root statement, evaluated in an empty
    /// binding context. Produces exactly one record. Covers the standard
    /// Bolt keep-alive pattern (`RETURN 1`) and any other constant-row
    /// projection. Wired into the parser in cycle 5 and into the
    /// compiler/executor in cycle 7 of the parser fix.
    ConstReturn(ConstReturnQuery),
    /// A DDL statement (index/constraint management). Parsed by
    /// `tessera-graph-cypher` and executed by the server's DDL handler;
    /// the core engine exposes only the schema catalog API.
    Ddl(DdlStatement),
    /// A `CALL <proc>() YIELD <col> [UNWIND …] [RETURN …]` statement (built-in
    /// procedure invocation). Parsed by `tessera-graph-cypher` and executed by
    /// the server's call handler; the core engine exposes only the data
    /// accessors (`node_labels`, `edge_types`). Boxed to keep the
    /// `GqlStatement` discriminant small (the payload carries two optional
    /// clause structs).
    Call(Box<CallStatement>),
}

/// A `RETURN <expr-list>` root statement.
///
/// Distinct from [`GqlQuery`] because `GqlQuery` assumes a non-empty
/// binding context (its `match_clause` is required). Reusing `GqlQuery`
/// with `Option<MatchClause>` would force every consumer that destructures
/// `GqlQuery` to handle the empty case; a dedicated type makes the
/// contract explicit at the dispatcher.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstReturnQuery {
    /// The expressions to project. At least one item is required by the
    /// parser; the executor emits exactly one record with one column per
    /// item.
    pub items: Vec<ReturnItem>,
    /// `RETURN DISTINCT ...` flag. Stored for fidelity with the surface
    /// syntax but ignored by the executor (one row is always distinct).
    pub distinct: bool,
    /// Optional `LIMIT <expr>`. Accepted for driver compatibility;
    /// harmless when the row count is already 1.
    pub limit: Option<Expr>,
    /// Optional `SKIP <expr>`. Accepted for driver compatibility;
    /// `SKIP 1` on a one-row stream yields zero rows.
    pub skip: Option<Expr>,
}

/// Admin statements: user management. Handled by the server's admin
/// dispatcher, never by the graph engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminStatement {
    CreateUser {
        username: String,
        password: SecretPlainPassword,
    },
    DropUser {
        username: String,
    },
    AlterUserPassword {
        username: String,
        password: SecretPlainPassword,
    },
    AlterUserStatus {
        username: String,
        enabled: bool,
    },
    AlterUserAdmin {
        username: String,
        is_admin: bool,
    },
    ShowUsers,
    /// `CREATE DATABASE <name> [IF NOT EXISTS] [WITH OPTIONS { ... }]`
    ///
    /// Parsed by `tessera-graph-cypher`, executed by the server admin
    /// handler (Task 8). Name validation (regex + reserved list) already
    /// happened at parse time; the handler re-validates against the
    /// auth store for defence-in-depth.
    CreateDatabase {
        name: String,
        if_not_exists: bool,
        options: DatabaseOptions,
    },
    /// `DROP DATABASE <name> [IF EXISTS]`
    DropDatabase {
        name: String,
        if_exists: bool,
    },
    /// `SHOW DATABASES` — lists the catalog entries visible to the
    /// caller. The admin handler filters by effective access.
    ShowDatabases,
    /// `GRANT {ACCESS|WRITE} ON DATABASE {<name>|*} TO <username>`
    ///
    /// Idempotent at the store: re-issuing a GRANT with the same
    /// `(username, target)` upgrades/downgrades `level` in place. The
    /// parser rejects invalid names and unknown levels; the handler
    /// (Task 9) enforces admin-only.
    Grant {
        username: String,
        target: GrantTargetAst,
        level: AccessLevelAst,
    },
    /// `REVOKE ACCESS ON DATABASE {<name>|*} FROM <username>`
    ///
    /// Removes the specific grant edge; the wildcard edge (if any)
    /// remains. Revoke on a non-existent pair is a no-op in the
    /// handler (spec §6.2).
    Revoke {
        username: String,
        target: GrantTargetAst,
    },
    /// `SHOW GRANTS [FOR <username>]`
    ///
    /// `filter_user=None` requires admin; `filter_user=Some(self)`
    /// is allowed without admin (the handler enforces this, not the
    /// parser).
    ShowGrants {
        filter_user: Option<String>,
    },
}

/// DDL statements: index and constraint management.
///
/// Parsed by `tessera-graph-cypher` and executed by the server's DDL
/// handler. The core engine exposes the [`crate::SchemaCatalog`] API; it
/// does not parse or dispatch these statements itself.
///
/// # `CREATE INDEX` semantics
/// `TesseraGraph`'s [`crate::PropertyIndex`] is a TOTAL index: every
/// property of every node is indexed unconditionally. `CREATE INDEX ON
/// :L(p)` therefore does NOT build new lookup structure — it only
/// RECORDS the declaration in the catalog so `SHOW INDEX INFO` lists it
/// and the client's schema-setup succeeds. The performance benefit the
/// client expects from an index is already present implicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdlStatement {
    /// `CREATE INDEX ON :Label(prop)` — legacy Cypher/Memgraph syntax.
    CreateIndexLegacy { label: String, prop: String },
    /// `CREATE INDEX FOR (n:Label) ON (n.prop)` — modern ISO GQL syntax.
    CreateIndexFor { label: String, prop: String },
    /// `DROP INDEX ON :Label(prop)` — removes a declared index.
    DropIndex { label: String, prop: String },
    /// `CREATE CONSTRAINT ON (n:Label) ASSERT n.prop IS UNIQUE`
    CreateUniqueConstraint { label: String, prop: String },
    /// `DROP CONSTRAINT ON (n:Label) ASSERT n.prop IS UNIQUE`
    DropConstraint { label: String, prop: String },
    /// `SHOW INDEX INFO` — tabular introspection (Memgraph shape).
    ShowIndexInfo,
    /// `SHOW CONSTRAINT INFO` — tabular introspection (Memgraph shape).
    ShowConstraintInfo,
    /// `ALTER LABEL :L SET APPEND ONLY` (`on = true`) or
    /// `ALTER LABEL :L REMOVE APPEND ONLY` (`on = false`).
    ///
    /// Unlike the index and constraint forms this has no Neo4j or Memgraph
    /// precedent to copy — append-only is this engine's own mode, so the syntax
    /// is chosen here (issue #61). `ALTER` because nothing is created: an
    /// existing label's schema changes. It also keeps parsing unambiguous,
    /// since `CREATE` already introduces node mutation.
    SetLabelAppendOnly { label: String, on: bool },
    /// `SHOW APPEND ONLY INFO` — tabular introspection, mirroring the index and
    /// constraint forms. A client that gets a write rejected needs a way to
    /// find out which labels are declared.
    ShowAppendOnlyInfo,
}

/// A `CALL <namespace>.<procedure>() YIELD <col> [UNWIND <col> AS <var>] [RETURN …]`
/// statement.
///
/// The pilot client issues the CALL, YIELD, UNWIND, and RETURN as one Bolt RUN
/// message, so the parser captures the whole statement. The server then:
/// 1. executes the built-in procedure to get a list value,
/// 2. optionally UNWINDs the list into per-element bindings,
/// 3. optionally projects a RETURN clause over those bindings.
///
/// Not `Eq`: `UnwindClause`/`ReturnClause` carry `Expr`, which contains `f64`
/// literals and is only `PartialEq` (mirrors `ReturnClause`/`PipelineTerminal`).
#[derive(Debug, Clone, PartialEq)]
pub struct CallStatement {
    /// The namespace component before the dot, e.g. `"mg"` or `"tessera"`.
    /// `None` means the caller wrote `CALL proc()` with no dot prefix.
    pub namespace: Option<String>,
    /// The bare procedure name after the dot, e.g. `"vertex_labels"`.
    pub procedure: String,
    /// Positional arguments inside the parentheses, e.g. the two string
    /// literals of `tessera.snapshot('mydb', '/dest')`. Empty for the
    /// introspection procedures (`vertex_labels`/`edge_types`), which take none.
    pub args: Vec<Expr>,
    /// The single column declared in `YIELD <col>`. For the two introspection
    /// procedures this equals the procedure name (`vertex_labels`/`edge_types`).
    /// Empty when the call has no `YIELD` clause (e.g. admin procedures whose
    /// result is dispatched by the server handler, not the read pipeline).
    pub yield_col: String,
    /// Optional trailing `UNWIND <yield_col> AS <var>` clause.
    pub unwind: Option<UnwindClause>,
    /// Optional trailing `RETURN <item>` clause.
    pub return_clause: Option<ReturnClause>,
}

/// Target of a `GRANT`/`REVOKE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantTargetAst {
    /// A specific database by name. Validated with the same regex +
    /// reserved-list as `CREATE DATABASE`.
    Named(String),
    /// `*` — a cross-database grant. The handler materialises this
    /// as a `(:User)-[:GRANTS]->(:Wildcard)` edge (spec §4.3).
    Wildcard,
}

/// Access level carried by a `GRANT` statement.
///
/// Subset of `tessera_graph_server::auth::AccessLevel` — `None` is
/// not expressible at parse time (it is the absence of a grant, not
/// a grantable value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLevelAst {
    /// `ACCESS` — maps to server-side `AccessLevel::Read`.
    Read,
    /// `WRITE` — maps to server-side `AccessLevel::ReadWrite`.
    ReadWrite,
}

/// Admin-statement-side options for `CREATE DATABASE ... WITH OPTIONS`.
///
/// Kept distinct from `tessera_graph_server::auth::DatabaseOptions` so
/// the core engine has no compile-time dependency on the server crate —
/// the admin handler converts between the two representations when
/// dispatching.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseOptions {
    pub max_size_bytes: Option<u64>,
    pub max_connections: Option<usize>,
}

/// Opaque plaintext-password carrier inside the AST. Redacts in
/// `Debug`; no `Display`, no `Serialize`, no `ToString`.
///
/// The AST is plaintext-only; hashing into PHC happens in the server's
/// admin handler after the statement has been parsed. Conversion to the
/// server-side `SecretString` wrapper happens at that seam.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretPlainPassword(Vec<u8>);

impl SecretPlainPassword {
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl std::fmt::Debug for SecretPlainPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretPlainPassword(<{} bytes>)", self.0.len())
    }
}

impl GqlStatement {
    /// Returns the inner [`MutationStatement`] if this is the `Mutation`
    /// variant, or `None` otherwise. Consumes `self`.
    #[must_use]
    pub fn into_mutation(self) -> Option<MutationStatement> {
        match self {
            Self::Mutation(ms) => Some(ms),
            Self::Query(_)
            | Self::Pipeline(_)
            | Self::Admin(_)
            | Self::ConstReturn(_)
            | Self::Ddl(_)
            | Self::Call(_) => None,
        }
    }

    /// Returns the inner [`GqlQuery`] if this is the `Query` variant,
    /// or `None` otherwise. Consumes `self`.
    #[must_use]
    pub fn into_query(self) -> Option<GqlQuery> {
        match self {
            Self::Query(q) => Some(q),
            Self::Mutation(_)
            | Self::Pipeline(_)
            | Self::Admin(_)
            | Self::ConstReturn(_)
            | Self::Ddl(_)
            | Self::Call(_) => None,
        }
    }

    /// Deprecated alias for [`Self::into_mutation`]. The previous name broke
    /// the Rust convention that `as_*` borrows rather than consumes.
    #[must_use]
    #[deprecated(
        since = "0.2.3",
        note = "renamed to `into_mutation` — this method consumes `self`"
    )]
    pub fn as_mutation(self) -> Option<MutationStatement> {
        self.into_mutation()
    }

    /// Deprecated alias for [`Self::into_query`].
    #[must_use]
    #[deprecated(
        since = "0.2.3",
        note = "renamed to `into_query` — this method consumes `self`"
    )]
    pub fn as_query(self) -> Option<GqlQuery> {
        self.into_query()
    }
}

/// A GQL mutation statement, optionally preceded by a MATCH and followed by a SET.
#[derive(Debug, Clone, PartialEq)]
pub struct MutationStatement {
    /// Optional UNWIND clause preceding the mutation.
    pub unwind_clause: Option<UnwindClause>,
    /// The optional MATCH clause that binds variables for the mutation.
    pub match_clause: Option<MatchClause>,
    /// The primary mutation operation.
    pub mutation: MutationClause,
    /// The optional SET clause applied after the mutation.
    pub set_clause: Option<SetClause>,
    /// Optional trailing `RETURN` projecting the mutated nodes back to the
    /// client (`MATCH (n) SET n = $m RETURN n`, `CREATE (n $m) RETURN n`).
    /// Each `RETURN`ed bound variable projects as a `GqlValue::Map` of the
    /// node's properties — the same shape MERGE's `return_var` produces.
    /// `None` for the bare `… SET …` / `CREATE …` forms with no projection.
    ///
    /// Boxed: a trailing RETURN is the uncommon case, so this keeps the
    /// `MutationStatement` (and thus `GqlStatement`) stack footprint at its
    /// prior baseline instead of growing every statement by the clause size.
    pub return_clause: Option<Box<ReturnClause>>,
}

/// The primary mutation operation within a [`MutationStatement`].
#[derive(Debug, Clone, PartialEq)]
pub enum MutationClause {
    /// A CREATE operation.
    Create(CreateClause),
    /// A SET operation (standalone, without a preceding CREATE/DELETE).
    Set(SetClause),
    /// A DELETE operation.
    Delete(DeleteClause),
    /// A MERGE operation.
    Merge(MergeClause),
}

/// A CREATE clause specifying one or more graph patterns to create.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateClause {
    /// The patterns to create.
    pub patterns: Vec<CreatePattern>,
}

/// A single pattern to be created: either a node or an edge.
#[derive(Debug, Clone, PartialEq)]
pub enum CreatePattern {
    /// A new node to create.
    Node {
        /// Optional variable binding.
        var: Option<String>,
        /// The node label.
        label: String,
        /// Inline property key-value pairs (expressions, not just literals).
        /// Empty when the whole-entity `prop_map` form is used.
        props: Vec<(String, Expr)>,
        /// Whole-entity property source: `CREATE (n:L $map)`. Mutually
        /// exclusive with `props`. `None` for the inline `{...}` form.
        prop_map: Option<Expr>,
    },
    /// A new directed edge between two already-bound variables.
    Edge {
        /// The source node variable.
        source_var: String,
        /// The relationship type label.
        rel_label: String,
        /// Inline relationship property key-value pairs (expressions, not just literals).
        rel_props: Vec<(String, Expr)>,
        /// The target node variable.
        target_var: String,
    },
}

impl CreatePattern {
    /// Returns the variable name for a node pattern, or `None` for edge patterns
    /// or nodes without a binding.
    #[must_use]
    pub fn var(&self) -> Option<&str> {
        match self {
            Self::Node { var, .. } => var.as_deref(),
            Self::Edge { .. } => None,
        }
    }
}

/// A SET clause assigning new values to properties on bound variables.
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    /// The list of property assignments.
    pub assignments: Vec<SetAssignment>,
}

/// A single assignment in a SET clause.
///
/// Three forms:
/// - `n.prop = expr` — per-property assignment.
/// - `n = $map`      — whole-entity overwrite from a map expression.
/// - `n += $map`     — whole-entity merge from a map (existing props preserved).
#[derive(Debug, Clone, PartialEq)]
pub enum SetAssignment {
    /// `var.prop = expr` — set a single named property.
    Property {
        /// The bound variable whose property is being set.
        var: String,
        /// The property key.
        prop: String,
        /// The new value expression.
        value: Expr,
    },
    /// `var = map_expr` — overwrite all properties from a map.
    EntityOverwrite {
        /// The bound variable.
        var: String,
        /// A `$param` (or other) map-valued expression.
        map_expr: Expr,
    },
    /// `var += map_expr` — merge properties from a map (existing preserved).
    EntityMerge {
        /// The bound variable.
        var: String,
        /// A `$param` (or other) map-valued expression.
        map_expr: Expr,
    },
}

/// A DELETE clause removing one or more bound variables from the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteClause {
    /// When `true`, detach-deletes nodes (removes incident edges first).
    pub detach: bool,
    /// The variables to delete.
    pub vars: Vec<String>,
}

/// A MERGE clause: match-or-create a node, then optionally apply per-branch
/// SET clauses and return the bound element.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClause {
    /// Optional variable binding for the merged node.
    pub var: Option<String>,
    /// The node label.
    pub label: String,
    /// Inline property key-value pairs used as the merge key. Values are
    /// expressions (not just literals) so `$param` is accepted.
    pub props: Vec<(String, Expr)>,
    /// `ON CREATE SET ...` — applied only when the node is newly created.
    pub on_create: Option<SetClause>,
    /// `ON MATCH SET ...` — applied only when the node already existed.
    pub on_match: Option<SetClause>,
    /// Variable to return after the merge (from a trailing `RETURN var`).
    pub return_var: Option<String>,
}

/// The quantifier kind of a list-predicate expression
/// ([`Expr::ListPredicate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListPredKind {
    /// `ALL` — the predicate holds for every element (vacuously `true` on an
    /// empty list).
    All,
    /// `ANY` — the predicate holds for at least one element (`false` on an
    /// empty list).
    Any,
    /// `NONE` — the predicate holds for no element (vacuously `true` on an
    /// empty list).
    None,
    /// `SINGLE` — the predicate holds for exactly one element (`false` on an
    /// empty list).
    Single,
}

/// Built-in aggregation functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    /// `COUNT`
    Count,
    /// `SUM`
    Sum,
    /// `AVG`
    Avg,
    /// `MIN`
    Min,
    /// `MAX`
    Max,
    /// `COLLECT`
    Collect,
}

// ── Pipeline AST ─────────────────────────────────────────────────────────────
//
// A `PipelineQuery` represents a multi-stage GQL statement such as
// `MATCH ... WITH ... [WITH ...] (RETURN | SET | CREATE | DELETE)`.
// It coexists with the legacy flat `GqlQuery` / `MutationStatement`: the parser
// picks the pipeline form only when at least one `WITH` appears in the input,
// so all existing tests that target the flat AST continue to work unchanged.

/// A multi-stage GQL statement with explicit WITH/UNWIND pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineQuery {
    /// Ordered sequence of stages. Always non-empty and starts with a `Match`
    /// stage in the statements produced by the parser (UNWIND-only pipelines
    /// are not in scope for the initial implementation).
    pub stages: Vec<PipelineStage>,
    /// The terminal clause that consumes the last stage's bindings.
    pub terminal: PipelineTerminal,
}

/// A single stage in a `PipelineQuery`.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineStage {
    /// `MATCH path_pattern (, path_pattern)* [WHERE expr]`.
    Match {
        /// The patterns to match.
        clause: MatchClause,
        /// Optional `WHERE` filter fused with this MATCH stage.
        where_clause: Option<WhereClause>,
    },
    /// `UNWIND expr AS var`.
    Unwind(UnwindClause),
    /// `WITH [DISTINCT] item (, item)* [WHERE expr] [ORDER BY ...] [SKIP n] [LIMIT n]`.
    With(WithClause),
}

/// A `WITH` projection/grouping stage.
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    /// `true` when the surface syntax is `WITH DISTINCT ...`.
    pub distinct: bool,
    /// Projected items — same shape as `RETURN` items (`expr [AS alias]`).
    pub items: Vec<ReturnItem>,
    /// Optional post-projection `WHERE` filter.
    pub where_clause: Option<WhereClause>,
    /// Optional `ORDER BY` applied after projection/aggregation.
    pub order_by: Option<OrderByClause>,
    /// Optional `SKIP n`, applied before LIMIT.
    pub skip: Option<SkipClause>,
    /// Optional `LIMIT n`.
    pub limit: Option<LimitClause>,
}

/// `SKIP n` — discard the first `n` rows of a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkipClause {
    /// Number of rows to skip.
    pub count: u64,
}

/// The terminal clause of a `PipelineQuery`.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineTerminal {
    /// `RETURN ...` — read-only result.
    Return {
        /// The projection spec.
        clause: ReturnClause,
        /// Optional `ORDER BY` on the projected rows.
        order_by: Option<OrderByClause>,
        /// Optional `SKIP n`.
        skip: Option<SkipClause>,
        /// Optional `LIMIT n`.
        limit: Option<LimitClause>,
    },
    /// `SET a.prop = expr [, ...]` — mutation against the final bindings.
    Set(SetClause),
    /// `CREATE pattern [, pattern]` — create from the final bindings.
    Create(CreateClause),
    /// `DELETE` / `DETACH DELETE` — delete using the final bindings.
    Delete(DeleteClause),
}

#[cfg(test)]
mod tests {
    use super::*;

    // 3d T2: CallStatement AST + GqlStatement::Call variant.
    #[test]
    fn call_statement_full_shape() {
        let stmt = GqlStatement::Call(Box::new(CallStatement {
            namespace: Some("mg".to_owned()),
            procedure: "vertex_labels".to_owned(),
            args: vec![],
            yield_col: "vertex_labels".to_owned(),
            unwind: Some(UnwindClause {
                expr: Expr::Var("vertex_labels".to_owned()),
                var: "vl".to_owned(),
            }),
            return_clause: Some(ReturnClause {
                distinct: false,
                items: vec![ReturnItem {
                    expr: Expr::Var("vl".to_owned()),
                    alias: None,
                }],
            }),
        }));
        let s = format!("{stmt:?}");
        assert!(s.contains("vertex_labels"), "got: {s}");
    }

    #[test]
    fn call_statement_yield_only_shape() {
        let stmt = GqlStatement::Call(Box::new(CallStatement {
            namespace: Some("tessera".to_owned()),
            procedure: "edge_types".to_owned(),
            args: vec![],
            yield_col: "edge_types".to_owned(),
            unwind: None,
            return_clause: None,
        }));
        let _ = format!("{stmt:?}");
    }

    #[test]
    fn gql_query_has_optional_clauses() {
        let q = GqlQuery {
            unwind_clause: None,
            match_clause: MatchClause {
                patterns: vec![],
                path_var: None,
            },
            where_clause: None,
            return_clause: ReturnClause {
                items: vec![],
                distinct: false,
            },
            group_by: None,
            order_by: None,
            limit: None,
        };
        assert!(q.where_clause.is_none());
        assert!(q.order_by.is_none());
        assert!(q.limit.is_none());
    }

    #[test]
    fn limit_clause_stores_value() {
        let lim = LimitClause { count: 42 };
        assert_eq!(lim.count, 42);
    }

    #[test]
    fn node_pattern_multi_label() {
        let np = NodePattern {
            var: Some("a".into()),
            labels: vec!["Person".into(), "Employee".into()],
            props: vec![],
        };
        assert_eq!(np.labels.len(), 2);
    }

    #[test]
    fn edge_direction_both() {
        let ep = EdgePattern {
            var: None,
            labels: vec![],
            props: vec![],
            direction: AstDirection::Both,
            length: EdgeLength::Fixed,
        };
        assert_eq!(ep.direction, AstDirection::Both);
    }

    #[test]
    fn edge_length_variable() {
        let len = EdgeLength::Variable {
            min: Some(1),
            max: Some(5),
        };
        match len {
            EdgeLength::Variable { min, max } => {
                assert_eq!(min, Some(1));
                assert_eq!(max, Some(5));
            }
            EdgeLength::Fixed => panic!("expected Variable"),
        }
    }

    #[test]
    fn path_pattern_with_hops() {
        let pp = PathPattern {
            start: NodePattern {
                var: Some("a".into()),
                labels: vec![],
                props: vec![],
            },
            hops: vec![],
        };
        assert!(pp.hops.is_empty());
    }

    #[test]
    fn expr_literal_int() {
        let e = Expr::Literal(Literal::Int(42));
        assert_eq!(e, Expr::Literal(Literal::Int(42)));
    }

    #[test]
    fn expr_property_access() {
        let e = Expr::PropAccess {
            var: "a".into(),
            prop: "name".into(),
        };
        match &e {
            Expr::PropAccess { var, prop } => {
                assert_eq!(var, "a");
                assert_eq!(prop, "name");
            }
            _ => panic!("expected PropAccess"),
        }
    }

    #[test]
    fn expr_binary_comparison() {
        let e = Expr::BinaryOp {
            left: Box::new(Expr::PropAccess {
                var: "a".into(),
                prop: "age".into(),
            }),
            op: BinOp::Gt,
            right: Box::new(Expr::Literal(Literal::Int(30))),
        };
        match e {
            Expr::BinaryOp { op, .. } => assert_eq!(op, BinOp::Gt),
            _ => panic!("expected BinaryOp"),
        }
    }

    #[test]
    fn expr_is_null() {
        let e = Expr::IsNull {
            expr: Box::new(Expr::PropAccess {
                var: "a".into(),
                prop: "x".into(),
            }),
            negated: false,
        };
        match e {
            Expr::IsNull { negated, .. } => assert!(!negated),
            _ => panic!("expected IsNull"),
        }
    }

    #[test]
    fn expr_aggregation_count_star() {
        let e = Expr::Aggregate {
            func: AggFunc::Count,
            arg: None,
        };
        match e {
            Expr::Aggregate { func, arg } => {
                assert_eq!(func, AggFunc::Count);
                assert!(arg.is_none());
            }
            _ => panic!("expected Aggregate"),
        }
    }

    #[test]
    fn gql_statement_wraps_query() {
        let q = GqlQuery {
            unwind_clause: None,
            match_clause: MatchClause {
                patterns: vec![],
                path_var: None,
            },
            where_clause: None,
            return_clause: ReturnClause {
                items: vec![],
                distinct: false,
            },
            group_by: None,
            order_by: None,
            limit: None,
        };
        let stmt = GqlStatement::Query(q.clone());
        assert_eq!(stmt, GqlStatement::Query(q));
    }

    #[test]
    fn mutation_statement_fields() {
        let create = CreateClause { patterns: vec![] };
        let stmt = MutationStatement {
            unwind_clause: None,
            match_clause: None,
            mutation: MutationClause::Create(create),
            set_clause: None,
            return_clause: None,
        };
        assert!(stmt.match_clause.is_none());
        assert!(stmt.set_clause.is_none());
        assert!(stmt.return_clause.is_none());
    }

    #[test]
    fn create_pattern_node_var() {
        let p = CreatePattern::Node {
            var: Some("n".into()),
            label: "Person".into(),
            props: vec![("name".into(), Expr::Literal(Literal::Str("Alice".into())))],
            prop_map: None,
        };
        assert_eq!(p.var(), Some("n"));
    }

    #[test]
    fn create_pattern_node_no_var() {
        let p = CreatePattern::Node {
            var: None,
            label: "Thing".into(),
            props: vec![],
            prop_map: None,
        };
        assert_eq!(p.var(), None);
    }

    #[test]
    fn create_pattern_edge_var_is_none() {
        let p = CreatePattern::Edge {
            source_var: "a".into(),
            rel_label: "KNOWS".into(),
            rel_props: vec![],
            target_var: "b".into(),
        };
        assert_eq!(p.var(), None);
    }

    #[test]
    fn set_clause_stores_assignments() {
        let asgn = SetAssignment::Property {
            var: "n".into(),
            prop: "age".into(),
            value: Expr::Literal(Literal::Int(30)),
        };
        let sc = SetClause {
            assignments: vec![asgn],
        };
        assert_eq!(sc.assignments.len(), 1);
        assert!(matches!(
            &sc.assignments[0],
            SetAssignment::Property { prop, .. } if prop == "age"
        ));
    }

    #[test]
    fn delete_clause_detach_flag() {
        let dc = DeleteClause {
            detach: true,
            vars: vec!["n".into()],
        };
        assert!(dc.detach);
        assert_eq!(dc.vars, vec!["n"]);
    }

    #[test]
    fn merge_clause_fields() {
        let mc = MergeClause {
            var: Some("n".into()),
            label: "Person".into(),
            props: vec![("name".into(), Expr::Literal(Literal::Str("Bob".into())))],
            on_create: None,
            on_match: None,
            return_var: None,
        };
        assert_eq!(mc.var.as_deref(), Some("n"));
        assert_eq!(mc.label, "Person");
        assert_eq!(mc.props.len(), 1);
    }

    #[test]
    fn mutation_clause_variants_accessible() {
        let set = SetClause {
            assignments: vec![SetAssignment::Property {
                var: "n".into(),
                prop: "x".into(),
                value: Expr::Literal(Literal::Int(1)),
            }],
        };
        let mc = MutationClause::Set(set);
        assert!(matches!(mc, MutationClause::Set(_)));

        let dc = MutationClause::Delete(DeleteClause {
            detach: false,
            vars: vec![],
        });
        assert!(matches!(dc, MutationClause::Delete(_)));

        let merge = MutationClause::Merge(MergeClause {
            var: None,
            label: "L".into(),
            props: vec![],
            on_create: None,
            on_match: None,
            return_var: None,
        });
        assert!(matches!(merge, MutationClause::Merge(_)));
    }

    // ── AST size guards ──────────────────────────────────────────────────────
    //
    // These tests pin the in-memory size of `Expr` and `GqlStatement` to the
    // current baseline (cycle 2 of the parser fix, before `Expr::ParamRef`
    // and `GqlStatement::ConstReturn` are added in cycle 3). Their purpose
    // is to detect AST growth that would shrink the per-frame stack budget
    // of the recursive-descent parser. Error log 2026-04-20 documented one
    // such regression: adding `Expr::Subscript` and `Expr::ListLit` pushed
    // the deepest-nesting parser tests into stack-overflow territory, which
    // had to be papered over with a `run_on_fat_stack` workaround.
    //
    // If a future change increases either size, the failing test forces an
    // explicit decision: box the new variant, accept the larger stack
    // frame, or increase the test-thread stack via `run_on_fat_stack`.
    //
    // Baseline (target x86_64-apple-darwin, rustc 1.85, edition 2024):
    //   size_of::<Expr>()         = 56 bytes
    //   size_of::<GqlStatement>() = 272 bytes
    //   size_of::<Literal>()      = 32 bytes
    //
    // 2026-06-07 (Fase 2 MERGE+maps): GqlStatement grew 256 → 272 (+16).
    // MergeClause gained `on_create`/`on_match: Option<SetClause>` +
    // `return_var: Option<String>`, and `CreatePattern::Node` gained
    // `prop_map: Option<Expr>`. The growth is a deliberate, spec-required AST
    // extension (not an accidental regression); the per-frame stack budget is
    // still ample, so the baseline is bumped rather than boxed.
    //
    // 2026-06-09 (Cycle 5.6, trailing RETURN): MutationStatement gained
    // `return_clause`. Measured unboxed it pushed GqlStatement to 304 (+32);
    // since a trailing RETURN is the uncommon mutation form, the field is
    // `Option<Box<ReturnClause>>` instead, so the growth is one pointer:
    // 272 → 280 (+8) rather than +32. Deliberate, minimal extension.
    //
    // 2026-06-13 (Fase B C5, path binding): MatchClause gained
    // `path_var: Option<String>` for `MATCH p = (…)`, growing GqlStatement
    // 280 → 304 (+24). Left unboxed: unlike the 5.6 case, `GqlStatement` is
    // built once at the top of parse_statement and is NOT on the deep
    // `parse_expr` recursion path (Expr stays 56 bytes), so the +24 does not
    // shrink the per-frame stack budget the guard protects. Boxing would add
    // indirection on every MATCH (the common path) to save bytes off the hot
    // recursion's reach — a worse trade than 5.6's. Deliberate, baseline bumped.

    #[test]
    fn ast_expr_size_is_pinned_at_baseline() {
        assert_eq!(
            std::mem::size_of::<Expr>(),
            56,
            "Expr size changed — see comment above ast_expr_size_is_pinned_at_baseline",
        );
    }

    #[test]
    fn ast_gql_statement_size_is_pinned_at_baseline() {
        assert_eq!(
            std::mem::size_of::<GqlStatement>(),
            304,
            "GqlStatement size changed — see comment above ast_expr_size_is_pinned_at_baseline",
        );
    }

    // ── Expr::ParamRef and GqlStatement::ConstReturn (parser fix cycle 3) ────

    #[test]
    fn ast_param_ref_named_constructs() {
        let p = Expr::ParamRef(ParamRef::Named("id".into()));
        assert_eq!(p, Expr::ParamRef(ParamRef::Named("id".into())));
    }

    #[test]
    fn ast_param_ref_positional_constructs() {
        let p = Expr::ParamRef(ParamRef::Positional(1));
        assert_eq!(p, Expr::ParamRef(ParamRef::Positional(1)));
    }

    #[test]
    fn ast_param_ref_named_and_positional_are_distinct() {
        // `$1` (positional) and `$"1"` (named with key "1") are different
        // syntactic forms even when they share a string key. The resolver
        // dispatches by variant, not by the key value alone.
        let named = Expr::ParamRef(ParamRef::Named("1".into()));
        let positional = Expr::ParamRef(ParamRef::Positional(1));
        assert_ne!(named, positional);
    }

    #[test]
    fn ast_const_return_query_constructs_with_single_item() {
        let q = ConstReturnQuery {
            items: vec![ReturnItem {
                expr: Expr::Literal(Literal::Int(1)),
                alias: None,
            }],
            distinct: false,
            limit: None,
            skip: None,
        };
        assert_eq!(q.items.len(), 1);
        assert!(!q.distinct);
        assert!(q.limit.is_none());
        assert!(q.skip.is_none());
        assert!(matches!(
            GqlStatement::ConstReturn(q),
            GqlStatement::ConstReturn(_)
        ));
    }

    // ── DDL AST (3c Task 1) ─────────────────────────────────────────────────

    #[test]
    fn ddl_statement_roundtrip_debug() {
        let stmt = GqlStatement::Ddl(DdlStatement::CreateIndexLegacy {
            label: "Person".to_owned(),
            prop: "id".to_owned(),
        });
        let s = format!("{stmt:?}");
        assert!(s.contains("CreateIndexLegacy"), "got: {s}");
    }

    #[test]
    fn ddl_statement_all_variants_debug() {
        let variants: &[DdlStatement] = &[
            DdlStatement::CreateIndexLegacy {
                label: "L".to_owned(),
                prop: "p".to_owned(),
            },
            DdlStatement::CreateIndexFor {
                label: "L".to_owned(),
                prop: "p".to_owned(),
            },
            DdlStatement::DropIndex {
                label: "L".to_owned(),
                prop: "p".to_owned(),
            },
            DdlStatement::CreateUniqueConstraint {
                label: "L".to_owned(),
                prop: "p".to_owned(),
            },
            DdlStatement::DropConstraint {
                label: "L".to_owned(),
                prop: "p".to_owned(),
            },
            DdlStatement::ShowIndexInfo,
            DdlStatement::ShowConstraintInfo,
        ];
        for v in variants {
            let _ = format!("{v:?}");
        }
    }
}
