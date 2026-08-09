// SPDX-License-Identifier: MIT

use std::fmt;

/// Unique identifier for nodes.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub(crate) u64);

/// Unique identifier for edges.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EdgeId(pub(crate) u64);

impl NodeId {
    /// Returns the underlying `u64` identifier.
    ///
    /// Useful for serialization or storing IDs in external data structures.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Creates a `NodeId` from a raw `u64` value.
    ///
    /// Useful for bridge crates and deserialization.
    #[must_use]
    pub const fn from_raw(v: u64) -> Self {
        Self(v)
    }
}

impl EdgeId {
    /// Returns the underlying `u64` identifier.
    ///
    /// Useful for serialization or storing IDs in external data structures.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Creates an `EdgeId` from a raw `u64` value.
    ///
    /// Useful for bridge crates and deserialization.
    #[must_use]
    pub const fn from_raw(v: u64) -> Self {
        Self(v)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.0)
    }
}

impl fmt::Display for EdgeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EdgeId({})", self.0)
    }
}

/// All errors produced by `TesseraGraph`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("node not found: {0}")]
    NodeNotFound(NodeId),

    #[error("edge not found: {0}")]
    EdgeNotFound(EdgeId),

    #[error("storage I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("corrupt page in {file} (page {page_id}): {reason}")]
    CorruptPage {
        file: &'static str,
        page_id: u32,
        reason: &'static str,
    },

    #[error(
        "checksum mismatch in {file} (page {page_id}): expected {expected:#010X}, got {actual:#010X}"
    )]
    ChecksumMismatch {
        file: &'static str,
        page_id: u32,
        expected: u32,
        actual: u32,
    },

    #[error("buffer pool exhausted: all pages are pinned")]
    BufferPoolExhausted,

    #[error("incompatible format version: found {found}, expected {expected}")]
    IncompatibleVersion { found: u16, expected: u16 },

    #[error("invalid magic bytes in {0}")]
    InvalidMagic(&'static str),

    #[error("store directory does not exist and create_if_missing is false")]
    NotPersisted,

    #[error("store was not closed cleanly (dirty flag set)")]
    DirtyStore,

    #[error("record too large: {size} bytes")]
    RecordTooLarge { size: usize },

    #[error("invalid string reference: offset {0}")]
    InvalidStringRef(u32),

    #[error("corrupt index file: {0}")]
    CorruptIndex(&'static str),

    #[error("WAL corrupt: {0}")]
    WalCorrupt(&'static str),

    #[error("pattern variable not found: {0}")]
    PatternVariableNotFound(String),

    #[error("invalid pattern: {0}")]
    InvalidPattern(String),

    #[error("GQL syntax error at line {line}, col {col}: {message}")]
    GqlSyntaxError {
        line: u32,
        col: u32,
        message: String,
    },

    #[error("GQL unsupported feature: {0}")]
    GqlUnsupported(String),

    #[error("GQL compile error: {0}")]
    GqlCompileError(String),

    #[error("GQL mutation error: {0}")]
    GqlMutationError(String),

    /// A `DELETE` (without `DETACH`) targeted a node that still has incident
    /// edges. Neo4j/Memgraph reject this: a node must have no relationships
    /// before it can be deleted, unless `DETACH DELETE` is used (which removes
    /// the incident edges first). This is a distinct typed variant so the Bolt
    /// handler can map it to a dedicated client-error code rather than folding
    /// it into the generic [`Error::GqlMutationError`] string. `node` is the
    /// node that could not be deleted; `relationships` is how many incident
    /// edges it still had at rejection time.
    #[error(
        "Cannot delete node {node}, because it still has relationships. \
         To delete this node, you must first delete its {relationships} relationships"
    )]
    DeleteConnectedNode { node: NodeId, relationships: usize },

    /// Multi-database (v0.5.0+) per-database write quota was reached.
    ///
    /// Returned by the optional quota hook installed via
    /// [`crate::Graph::open_with_hook`] when the database directory has
    /// reached or exceeded its configured `max_size_bytes`. The check
    /// fires BEFORE any in-memory mutation or WAL append (decision C'):
    /// when this error is produced, the would-be write was rejected
    /// cleanly and nothing was persisted.
    ///
    /// `path` is the database directory captured by the hook closure;
    /// `limit_bytes` is the configured quota; `current_bytes` is the
    /// observed on-disk size at the moment of rejection.
    #[error(
        "quota exceeded for database at {path}: limit {limit_bytes} bytes, \
         current {current_bytes} bytes"
    )]
    QuotaExceeded {
        path: String,
        limit_bytes: u64,
        current_bytes: u64,
    },

    /// A write was rejected because a unique constraint exists on
    /// `(label, prop)` and the value already exists in the graph. The server
    /// maps this to the Bolt wire code
    /// `Neo.ClientError.Schema.ConstraintValidationFailed`.
    #[error(
        "constraint violation: unique constraint on :{label}({prop}) \
         already has value {value}"
    )]
    ConstraintViolation {
        label: String,
        prop: String,
        value: String,
    },

    /// A physical backup (snapshot or restore) operation failed validation:
    /// the source directory is not a valid database (missing `graph.meta`), the
    /// destination is invalid, or a required file is absent. I/O failures
    /// during the copy itself surface as [`Error::Io`]; this variant is for
    /// domain-level validation of the backup contract.
    #[error("backup error: {0}")]
    Backup(String),

    /// A transaction operation (`begin_txn`/`commit_txn`/`rollback_txn` or a
    /// `*_in_txn` mutation) was invoked on a [`crate::Graph`] that has not
    /// called [`crate::Graph::enable_mvcc`]. Explicit transactions require MVCC
    /// mode; legacy single-version graphs reject them rather than silently
    /// auto-committing.
    #[error("MVCC is not enabled on this graph; call Graph::enable_mvcc first")]
    MvccNotEnabled,

    /// A transaction operation named a `txn_id` that is not in the active
    /// registry — it was never begun, or was already committed or rolled back.
    #[error("transaction {0} is not active")]
    TxnNotActive(u64),

    /// A transaction's uncommitted delta chain exceeded the configured
    /// per-transaction memory cap ([`crate::Graph::set_txn_memory_cap`]) and was
    /// aborted (equivalent to an implicit rollback). `used_bytes` is the
    /// estimated size the transaction would have reached; `cap_bytes` is the
    /// configured limit.
    #[error(
        "transaction {txn_id} exceeded the memory cap ({used_bytes} > {cap_bytes} bytes) and was aborted"
    )]
    TxnMemoryCapExceeded {
        txn_id: u64,
        used_bytes: u64,
        cap_bytes: u64,
    },

    /// A write inside an explicit transaction targeted a node whose label was
    /// declared append-only (issue #43). These nodes are exempt from MVCC
    /// visibility resolution on read, which only holds because they never
    /// acquire a delta chain — so transactional writes to them are refused
    /// rather than silently breaking the read fast path.
    #[error("label '{label}' is append-only and cannot be written inside a transaction")]
    AppendOnlyLabelInTxn { label: String },

    /// A batch (see [`crate::Graph::begin_batch`]) accumulated more operations
    /// or estimated bytes than the configured cap
    /// ([`crate::Graph::set_batch_limits`]) allows. The mutation that would have
    /// breached the cap was rejected BEFORE any state change — unlike
    /// [`Error::TxnMemoryCapExceeded`], the batch is NOT rolled back (batches
    /// are not atomic; prior mutations in this batch remain applied). `current`
    /// is the value the counter would have reached; `limit` is the configured
    /// cap.
    #[error("batch {kind} limit exceeded ({current} > {limit} {unit})", unit = kind.unit())]
    BatchLimitExceeded {
        kind: BatchLimitKind,
        current: u64,
        limit: u64,
    },
}

/// Which batch cap was breached — see [`Error::BatchLimitExceeded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchLimitKind {
    /// The operation-count cap ([`crate::Graph::set_batch_limits`], `max_ops`).
    Operations,
    /// The estimated-byte cap ([`crate::Graph::set_batch_limits`], `max_bytes`).
    Bytes,
}

impl BatchLimitKind {
    /// The plural unit noun used in the error message (`operations` / `bytes`).
    const fn unit(self) -> &'static str {
        match self {
            Self::Operations => "operations",
            Self::Bytes => "bytes",
        }
    }
}

impl std::fmt::Display for BatchLimitKind {
    /// The singular adjective used in the error message (`operation` / `memory`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Operations => write!(f, "operation"),
            Self::Bytes => write!(f, "memory"),
        }
    }
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gql_error_variants_display() {
        let e = Error::GqlSyntaxError {
            line: 1,
            col: 5,
            message: "unexpected token".into(),
        };
        assert!(e.to_string().contains("line 1"));
        assert!(e.to_string().contains("col 5"));

        let e2 = Error::GqlUnsupported("CREATE".into());
        assert!(e2.to_string().contains("CREATE"));

        let e3 = Error::GqlCompileError("unresolved variable 'x'".into());
        assert!(e3.to_string().contains("unresolved variable"));

        let e4 = Error::GqlMutationError("cannot delete non-existent node".into());
        assert_eq!(
            e4.to_string(),
            "GQL mutation error: cannot delete non-existent node"
        );
    }

    #[test]
    fn delete_connected_node_error_displays_neo4j_style_message() {
        let e = Error::DeleteConnectedNode {
            node: NodeId(7),
            relationships: 3,
        };
        let msg = e.to_string();
        assert!(msg.contains("Cannot delete node"), "message: {msg}");
        assert!(msg.contains('7'), "message: {msg}");
        assert!(msg.contains("still has relationships"), "message: {msg}");
    }

    #[test]
    fn batch_limit_error_variants_display() {
        assert_eq!(
            Error::BatchLimitExceeded {
                kind: BatchLimitKind::Operations,
                current: 101,
                limit: 100,
            }
            .to_string(),
            "batch operation limit exceeded (101 > 100 operations)"
        );
        assert_eq!(
            Error::BatchLimitExceeded {
                kind: BatchLimitKind::Bytes,
                current: 2048,
                limit: 1024,
            }
            .to_string(),
            "batch memory limit exceeded (2048 > 1024 bytes)"
        );
    }

    #[test]
    fn mvcc_error_variants_display() {
        assert!(
            Error::MvccNotEnabled
                .to_string()
                .contains("MVCC is not enabled")
        );
        assert_eq!(
            Error::TxnNotActive(7).to_string(),
            "transaction 7 is not active"
        );
        assert_eq!(
            Error::TxnMemoryCapExceeded {
                txn_id: 3,
                used_bytes: 300,
                cap_bytes: 200
            }
            .to_string(),
            "transaction 3 exceeded the memory cap (300 > 200 bytes) and was aborted"
        );
    }
}
