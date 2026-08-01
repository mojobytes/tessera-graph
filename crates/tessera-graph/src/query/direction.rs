// SPDX-License-Identifier: Apache-2.0

/// Direction filter for graph traversals and neighbor queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Follow outgoing edges only.
    Outgoing,
    /// Follow incoming edges only.
    Incoming,
    /// Follow edges in both directions.
    Both,
}
