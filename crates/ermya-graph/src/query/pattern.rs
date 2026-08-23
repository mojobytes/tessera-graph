// SPDX-License-Identifier: MIT

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::access::GraphAccess;
use crate::edge::Edge;
use crate::error::{Error, NodeId, Result};
use crate::node::Node;
use crate::property::Property;
use crate::query::direction::Direction;
use crate::query::neighbor::NeighborQuery;

/// Partial match state during pattern execution.
///
/// Uses `Arc` to allow O(1) clone of bindings when expanding hops — the inner
/// `HashMap` is only deep-copied (via `Arc::make_mut`) when a new binding is
/// actually inserted.
type PartialMatch = (Arc<HashMap<String, Node>>, Arc<HashMap<String, Edge>>);

/// A single match result from a pattern query.
///
/// Contains bindings from variable names to the matched nodes and edges.
#[derive(Debug, Clone)]
pub struct PatternMatch {
    nodes: HashMap<String, Node>,
    edges: HashMap<String, Edge>,
}

impl PatternMatch {
    /// Creates a new `PatternMatch` from node and edge bindings.
    #[must_use]
    pub(crate) const fn new(nodes: HashMap<String, Node>, edges: HashMap<String, Edge>) -> Self {
        Self { nodes, edges }
    }

    /// Creates an empty `PatternMatch` with no bindings.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: HashMap::new(),
        }
    }

    /// Clones the node bindings map.
    #[must_use]
    pub(crate) fn nodes_clone(&self) -> HashMap<String, Node> {
        self.nodes.clone()
    }

    /// Clones the edge bindings map.
    #[must_use]
    pub(crate) fn edges_clone(&self) -> HashMap<String, Edge> {
        self.edges.clone()
    }

    /// Returns an iterator over the names of the bound node variables.
    /// Cheaper than [`Self::nodes_clone`] when the caller only needs the
    /// variable names (e.g. DISTINCT / GROUP BY key construction).
    pub(crate) fn node_vars(&self) -> impl Iterator<Item = &str> {
        self.nodes.keys().map(String::as_str)
    }

    /// Returns an iterator over the names of the bound edge variables.
    pub(crate) fn edge_vars(&self) -> impl Iterator<Item = &str> {
        self.edges.keys().map(String::as_str)
    }

    /// Merge two `PatternMatch` bindings into one (for cross-join).
    ///
    /// Node and edge bindings from `other` are added to `self`'s bindings.
    /// If both contain the same variable, `other`'s value wins.
    #[must_use]
    pub fn merge(&self, other: &Self) -> Self {
        let mut nodes = self.nodes.clone();
        nodes.extend(other.nodes.iter().map(|(k, v)| (k.clone(), v.clone())));
        let mut edges = self.edges.clone();
        edges.extend(other.edges.iter().map(|(k, v)| (k.clone(), v.clone())));
        Self { nodes, edges }
    }

    /// Returns the node bound to the given variable name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PatternVariableNotFound`] if no node is bound to `var`.
    pub fn get_node(&self, var: &str) -> Result<&Node> {
        self.nodes
            .get(var)
            .ok_or_else(|| Error::PatternVariableNotFound(var.to_owned()))
    }

    /// Returns the edge bound to the given variable name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PatternVariableNotFound`] if no edge is bound to `var`.
    pub fn get_edge(&self, var: &str) -> Result<&Edge> {
        self.edges
            .get(var)
            .ok_or_else(|| Error::PatternVariableNotFound(var.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// Pattern steps (internal IR)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NodeConstraint {
    pub(crate) var: String,
    pub(crate) label: Option<String>,
    pub(crate) props: Vec<(String, Property)>,
}

#[derive(Debug, Clone)]
struct EdgeConstraint {
    var: Option<String>,
    direction: Direction,
    label: Option<String>,
    props: Vec<(String, Property)>,
}

#[derive(Debug, Clone)]
enum PatternStep {
    Node(NodeConstraint),
    Edge(EdgeConstraint),
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for graph pattern queries.
///
/// Chains `.node()` and `.edge()` calls to describe a pattern, then
/// `.execute()` to find all matches in the graph.
///
/// # Example
///
/// ```
/// use ermya_graph::{Graph, Properties, Direction, props};
///
/// let mut g = Graph::new();
/// let alice = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
/// let bob   = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
/// let cats  = g.add_node("Thing",  props! { "name" => "Cats" }).unwrap();
/// g.add_edge("KNOWS", alice, bob, Properties::new()).unwrap();
/// g.add_edge("LIKES", bob, cats, Properties::new()).unwrap();
///
/// let results: Vec<_> = g.pattern()
///     .node("a").label("Person")
///     .edge(Direction::Outgoing).label("KNOWS")
///     .node("b")
///     .edge(Direction::Outgoing).label("LIKES")
///     .node("c").label("Thing").where_prop("name", "Cats")
///     .execute()
///     .unwrap()
///     .collect::<ermya_graph::Result<Vec<_>>>()
///     .unwrap();
///
/// assert_eq!(results.len(), 1);
/// assert_eq!(results[0].get_node("a").unwrap().id(), alice);
/// assert_eq!(results[0].get_node("c").unwrap().id(), cats);
/// ```
pub struct PatternBuilder<'g, G: GraphAccess + ?Sized> {
    graph: &'g G,
    steps: Vec<PatternStep>,
    /// When `Some`, only these property keys (plus constraint keys) are loaded.
    /// When `None`, all properties are loaded (full node read).
    projection: Option<Vec<String>>,
}

impl<'g, G: GraphAccess + ?Sized> PatternBuilder<'g, G> {
    /// Creates a new pattern builder on the given graph.
    pub const fn new(graph: &'g G) -> Self {
        Self {
            graph,
            steps: Vec::new(),
            projection: None,
        }
    }

    /// Sets the property projection for this pattern.
    ///
    /// When set, only these property keys (merged with constraint keys from
    /// `where_prop`) are loaded from disk. Properties not in this set will
    /// not appear in matched nodes.
    ///
    /// When not set (default), all properties are loaded — this is the
    /// correct behavior when the caller (e.g. GQL compiler) needs arbitrary
    /// property access after matching.
    #[must_use]
    pub fn project(mut self, keys: Vec<String>) -> Self {
        self.projection = Some(keys);
        self
    }

    /// Adds a node step with the given variable name.
    #[must_use]
    pub fn node(mut self, var: impl Into<String>) -> Self {
        self.steps.push(PatternStep::Node(NodeConstraint {
            var: var.into(),
            label: None,
            props: Vec::new(),
        }));
        self
    }

    /// Adds an unnamed edge step with the given direction.
    #[must_use]
    pub fn edge(mut self, direction: Direction) -> Self {
        self.steps.push(PatternStep::Edge(EdgeConstraint {
            var: None,
            direction,
            label: None,
            props: Vec::new(),
        }));
        self
    }

    /// Adds a named edge step with the given variable and direction.
    #[must_use]
    pub fn edge_var(mut self, var: impl Into<String>, direction: Direction) -> Self {
        self.steps.push(PatternStep::Edge(EdgeConstraint {
            var: Some(var.into()),
            direction,
            label: None,
            props: Vec::new(),
        }));
        self
    }

    /// Sets the label constraint on the last added step (node or edge).
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        let label = label.into();
        match self.steps.last_mut() {
            Some(PatternStep::Node(c)) => c.label = Some(label),
            Some(PatternStep::Edge(c)) => c.label = Some(label),
            None => {}
        }
        self
    }

    /// Adds a property equality filter to the last node step.
    ///
    /// Multiple calls are combined with AND semantics.
    #[must_use]
    pub fn where_prop(mut self, key: impl Into<String>, value: impl Into<Property>) -> Self {
        if let Some(PatternStep::Node(c)) = self.steps.last_mut() {
            c.props.push((key.into(), value.into()));
        }
        self
    }

    /// Adds a property equality filter to the last edge step.
    ///
    /// Multiple calls are combined with AND semantics.
    #[must_use]
    pub fn where_edge_prop(mut self, key: impl Into<String>, value: impl Into<Property>) -> Self {
        if let Some(PatternStep::Edge(c)) = self.steps.last_mut() {
            c.props.push((key.into(), value.into()));
        }
        self
    }

    /// Executes the pattern and returns an iterator over all matches.
    ///
    /// Structural validation (malformed patterns, duplicate variables) is
    /// performed eagerly and returned as `Err`. Storage errors encountered
    /// during iteration are yielded as `Err` items from the iterator.
    ///
    /// For zero-hop patterns (single node), candidates are loaded lazily
    /// one at a time. For multi-hop patterns, hops are expanded eagerly
    /// per hop (required by the join algorithm) but results are yielded
    /// lazily from the final set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPattern`] if the pattern structure is malformed
    /// (e.g. duplicate variable names, consecutive nodes without an edge).
    pub fn execute(self) -> Result<PatternMatchIter<'g, G>> {
        self.validate_no_empty_vars()?;
        let segment = self.parse_segment()?;

        let Some(segment) = segment else {
            return Ok(PatternMatchIter::empty());
        };

        Self::validate_unique_vars(&segment)?;

        if segment.hops.is_empty() {
            // Zero-hop: lazy iteration over candidates.
            // When label + property are available, narrows via property index.
            let ids = narrow_candidates(self.graph, &segment.node);

            return Ok(PatternMatchIter::lazy_candidates(
                self.graph,
                ids,
                segment.node,
                self.projection,
            ));
        }

        // Multi-hop: eager expansion, lazy emission
        let candidates = self.initial_candidates(&segment.node)?;

        let mut current: Vec<PartialMatch> = candidates
            .into_iter()
            .map(|node| {
                let mut nodes = HashMap::new();
                nodes.insert(segment.node.var.clone(), node);
                (Arc::new(nodes), Arc::new(HashMap::new()))
            })
            .collect();

        let mut next: Vec<PartialMatch> = Vec::new();

        for (hop_idx, hop) in segment.hops.iter().enumerate() {
            let prev_var = Self::prev_node_var(hop_idx, &segment.node, &segment.hops);
            next.clear();
            for (node_bindings, edge_bindings) in &current {
                let prev_node = node_bindings
                    .get(prev_var)
                    .ok_or_else(|| Error::PatternVariableNotFound(prev_var.to_owned()))?;
                let expanded =
                    self.expand_hop(prev_node.id(), hop, node_bindings, edge_bindings)?;
                next.extend(expanded);
            }
            std::mem::swap(&mut current, &mut next);
        }

        Ok(PatternMatchIter::materialized(current))
    }

    // ------------------------------------------------------------------
    // Internal: parsing and validation
    // ------------------------------------------------------------------

    /// Parses steps into a single segment (linear path pattern).
    ///
    /// Returns `Err` if the pattern is structurally malformed (e.g. consecutive
    /// nodes without an edge, or an edge not followed by a node).
    fn parse_segment(&self) -> Result<Option<Segment>> {
        let mut iter = self.steps.iter();

        // Expect the first step to be a Node.
        let first_node = match iter.next() {
            Some(PatternStep::Node(c)) => c.clone(),
            Some(PatternStep::Edge(_)) => {
                return Err(Error::InvalidPattern(
                    "pattern must start with a node step".into(),
                ));
            }
            None => return Ok(None),
        };

        let mut hops = Vec::new();
        let mut expect_edge = true;

        for step in iter {
            match (expect_edge, step) {
                (true, PatternStep::Edge(ec)) => {
                    hops.push(Hop {
                        edge: ec.clone(),
                        node: NodeConstraint {
                            var: String::new(),
                            label: None,
                            props: Vec::new(),
                        },
                    });
                    expect_edge = false;
                }
                (false, PatternStep::Node(nc)) => {
                    // Fill in the node part of the last hop.
                    if let Some(last) = hops.last_mut() {
                        last.node = nc.clone();
                    }
                    expect_edge = true;
                }
                (true, PatternStep::Node(_)) => {
                    return Err(Error::InvalidPattern(
                        "consecutive node steps without an edge".into(),
                    ));
                }
                (false, PatternStep::Edge(_)) => {
                    return Err(Error::InvalidPattern(
                        "consecutive edge steps without a node".into(),
                    ));
                }
            }
        }

        // If the last step was an edge without a following node, that's malformed.
        if !expect_edge {
            return Err(Error::InvalidPattern(
                "pattern ends with an edge step (missing final node)".into(),
            ));
        }

        Ok(Some(Segment {
            node: first_node,
            hops,
        }))
    }

    /// Validates that all node and edge variable names are unique.
    fn validate_unique_vars(segment: &Segment) -> Result<()> {
        let mut seen = HashSet::new();
        if !seen.insert(segment.node.var.as_str()) {
            return Err(Error::InvalidPattern(format!(
                "duplicate node variable: '{}'",
                segment.node.var
            )));
        }
        for hop in &segment.hops {
            if !seen.insert(hop.node.var.as_str()) {
                return Err(Error::InvalidPattern(format!(
                    "duplicate node variable: '{}'",
                    hop.node.var
                )));
            }
            if let Some(ref var) = hop.edge.var
                && !seen.insert(var.as_str())
            {
                return Err(Error::InvalidPattern(format!(
                    "duplicate edge variable: '{var}'"
                )));
            }
        }
        Ok(())
    }

    /// Rejects empty variable names in user-provided steps.
    fn validate_no_empty_vars(&self) -> Result<()> {
        for step in &self.steps {
            match step {
                PatternStep::Node(c) if c.var.is_empty() => {
                    return Err(Error::InvalidPattern(
                        "empty node variable name is not allowed; use a descriptive name".into(),
                    ));
                }
                PatternStep::Edge(c) if c.var.as_deref() == Some("") => {
                    return Err(Error::InvalidPattern(
                        "empty edge variable name is not allowed".into(),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Gets initial candidates for a node constraint.
    ///
    /// When the constraint has both a label and at least one property,
    /// narrows candidates via the property index (O(1)) instead of
    /// scanning all nodes of that label.
    fn initial_candidates(&self, constraint: &NodeConstraint) -> Result<Vec<Node>> {
        let ids = narrow_candidates(self.graph, constraint);

        let mut nodes = Vec::with_capacity(ids.len());
        for id in ids {
            // A candidate id comes from the label/property index, which under
            // MVCC keeps a committed-deleted node until the vacuum removes it
            // (the delete's category-B cleanup runs in the vacuum, not at
            // commit). Such an id is no longer visible to this read's snapshot,
            // so `node_visible` is false — skip it rather than propagate the
            // `NodeNotFound` that reading it would raise. A genuine storage
            // error on a visible node still propagates via `?`.
            if !self.graph.node_visible(id) {
                continue;
            }
            let node = self.load_node(id, constraint)?;
            if node_matches(&node, constraint) {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    /// Returns the variable name of the node preceding the hop at `hop_idx`.
    fn prev_node_var<'a>(
        hop_idx: usize,
        first_node: &'a NodeConstraint,
        hops: &'a [Hop],
    ) -> &'a str {
        if hop_idx == 0 {
            &first_node.var
        } else {
            &hops[hop_idx - 1].node.var
        }
    }

    /// Loads a node, applying projection if configured.
    fn load_node(&self, id: NodeId, constraint: &NodeConstraint) -> Result<Node> {
        load_node_projected(self.graph, id, constraint, self.projection.as_deref())
    }

    /// Expands one (edge, node) hop from a source node, returning new matches.
    fn expand_hop(
        &self,
        source: NodeId,
        hop: &Hop,
        prev_nodes: &Arc<HashMap<String, Node>>,
        prev_edges: &Arc<HashMap<String, Edge>>,
    ) -> Result<Vec<PartialMatch>> {
        let mut query = NeighborQuery::new(self.graph, source).direction(hop.edge.direction);
        if let Some(ref label) = hop.edge.label {
            query = query.label(label.as_str());
        }
        let edges = query.collect()?;

        let mut results = Vec::new();
        for edge in edges {
            if !edge_matches(&edge, &hop.edge) {
                continue;
            }

            let neighbor_id = if edge.source() == source {
                edge.target()
            } else {
                edge.source()
            };

            // Fast-path: when only a label constraint exists (no property
            // constraints), check the label alone to skip full node
            // deserialization for non-matching nodes.
            if let Some(ref required_label) = hop.node.label
                && hop.node.props.is_empty()
            {
                let actual = self.graph.node_label(neighbor_id)?;
                if actual != *required_label {
                    continue;
                }
            }

            let neighbor = self.load_node(neighbor_id, &hop.node)?;
            if !node_matches(&neighbor, &hop.node) {
                continue;
            }

            let mut new_nodes = Arc::clone(prev_nodes);
            Arc::make_mut(&mut new_nodes).insert(hop.node.var.clone(), neighbor);

            let mut new_edges = Arc::clone(prev_edges);
            if let Some(ref var) = hop.edge.var {
                Arc::make_mut(&mut new_edges).insert(var.clone(), edge);
            }

            results.push((new_nodes, new_edges));
        }

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Narrows the initial candidate set for a node constraint.
///
/// When both a label and property constraints exist, intersects the results
/// from ALL property indices rather than only using the first property.
/// For a node to be a candidate it must match every property constraint,
/// so intersecting the per-property index sets gives a tight upper bound
/// before the final `node_matches` check.
///
/// Falls back to `nodes_by_label` (label-only index) when there are no
/// property constraints, or to `node_ids` (full scan) when no label is
/// provided.
pub fn narrow_candidates<G: GraphAccess + ?Sized>(
    graph: &G,
    constraint: &NodeConstraint,
) -> Vec<NodeId> {
    let Some(label) = constraint.label.as_ref() else {
        return graph.node_ids();
    };

    if constraint.props.is_empty() {
        return graph.nodes_by_label(label);
    }

    // Start with first property's result set.
    let (first_key, first_value) = &constraint.props[0];
    let mut candidates: HashSet<NodeId> = graph
        .nodes_by_label_and_property(label, first_key, first_value)
        .into_iter()
        .collect();

    // Intersect with each subsequent property's result set.
    for (key, value) in constraint.props.iter().skip(1) {
        if candidates.is_empty() {
            break;
        }
        let next: HashSet<NodeId> = graph
            .nodes_by_label_and_property(label, key, value)
            .into_iter()
            .collect();
        candidates.retain(|id| next.contains(id));
    }

    candidates.into_iter().collect()
}

/// Extracts the property keys from a node constraint for projected reads.
fn constraint_keys(constraint: &NodeConstraint) -> Vec<&str> {
    constraint.props.iter().map(|(k, _)| k.as_str()).collect()
}

/// Loads a node with optional projection applied.
///
/// When `projection` is `Some`, only constraint keys + projection keys are loaded.
/// When `None`, all properties are loaded (full read).
fn load_node_projected<G: GraphAccess + ?Sized>(
    graph: &G,
    id: NodeId,
    constraint: &NodeConstraint,
    projection: Option<&[String]>,
) -> Result<Node> {
    projection.map_or_else(
        || graph.node(id),
        |extra_keys| {
            let mut keys: Vec<&str> = constraint_keys(constraint);
            keys.extend(extra_keys.iter().map(String::as_str));
            keys.sort_unstable();
            keys.dedup();
            graph.node_projected(id, &keys)
        },
    )
}

/// Checks if a node satisfies a constraint's label and property filters.
fn node_matches(node: &Node, constraint: &NodeConstraint) -> bool {
    if let Some(ref label) = constraint.label
        && node.label() != label
    {
        return false;
    }
    for (key, expected) in &constraint.props {
        match node.properties().get(key) {
            Some(actual) if actual == expected => {}
            _ => return false,
        }
    }
    true
}

/// Checks if an edge satisfies a constraint's property filters.
///
/// Label filtering is already handled by the `NeighborQuery`, so this only
/// checks property predicates.
fn edge_matches(edge: &Edge, constraint: &EdgeConstraint) -> bool {
    for (key, expected) in &constraint.props {
        match edge.properties().get(key) {
            Some(actual) if actual == expected => {}
            _ => return false,
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Pattern match iterator
// ---------------------------------------------------------------------------

/// Iterator over pattern match results.
///
/// Created by [`PatternBuilder::execute`]. Yields `Result<PatternMatch>` items —
/// structural errors are caught at construction time, but storage errors during
/// lazy candidate loading appear as `Err` items.
pub struct PatternMatchIter<'g, G: GraphAccess + ?Sized> {
    state: IterState<'g, G>,
}

impl<G: GraphAccess + ?Sized> std::fmt::Debug for PatternMatchIter<'_, G> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatternMatchIter").finish_non_exhaustive()
    }
}

enum IterState<'g, G: GraphAccess + ?Sized> {
    /// Empty pattern — no results.
    Empty,
    /// Zero-hop: lazy iteration over candidate node IDs.
    LazyCandidates {
        graph: &'g G,
        ids: std::vec::IntoIter<NodeId>,
        constraint: NodeConstraint,
        projection: Option<Vec<String>>,
    },
    /// Multi-hop: pre-materialized partial matches, yielded lazily.
    Materialized {
        inner: std::vec::IntoIter<PartialMatch>,
    },
}

impl<'g, G: GraphAccess + ?Sized> PatternMatchIter<'g, G> {
    const fn empty() -> Self {
        Self {
            state: IterState::Empty,
        }
    }

    fn lazy_candidates(
        graph: &'g G,
        ids: Vec<NodeId>,
        constraint: NodeConstraint,
        projection: Option<Vec<String>>,
    ) -> Self {
        Self {
            state: IterState::LazyCandidates {
                graph,
                ids: ids.into_iter(),
                constraint,
                projection,
            },
        }
    }

    fn materialized(matches: Vec<PartialMatch>) -> Self {
        Self {
            state: IterState::Materialized {
                inner: matches.into_iter(),
            },
        }
    }
}

impl<G: GraphAccess + ?Sized> Iterator for PatternMatchIter<'_, G> {
    type Item = Result<PatternMatch>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.state {
            IterState::Empty => None,
            IterState::LazyCandidates {
                graph,
                ids,
                constraint,
                projection,
            } => {
                for id in ids.by_ref() {
                    // The candidate ids come from the label/property index, which
                    // under MVCC keeps a committed-deleted node until the vacuum
                    // removes it. Such an id is no longer visible to this read's
                    // snapshot — skip it rather than propagate the `NodeNotFound`
                    // that reading it would raise. A genuine storage error on a
                    // visible node still propagates below.
                    if !graph.node_visible(id) {
                        continue;
                    }
                    let node = load_node_projected(*graph, id, constraint, projection.as_deref());
                    match node {
                        Ok(n) if node_matches(&n, constraint) => {
                            let mut nodes = HashMap::new();
                            nodes.insert(constraint.var.clone(), n);
                            return Some(Ok(PatternMatch {
                                nodes,
                                edges: HashMap::new(),
                            }));
                        }
                        Ok(_) => {}
                        Err(e) => return Some(Err(e)),
                    }
                }
                None
            }
            IterState::Materialized { inner } => inner.next().map(|(nodes, edges)| {
                let nodes = Arc::try_unwrap(nodes).unwrap_or_else(|arc| (*arc).clone());
                let edges = Arc::try_unwrap(edges).unwrap_or_else(|arc| (*arc).clone());
                Ok(PatternMatch { nodes, edges })
            }),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.state {
            IterState::Empty => (0, Some(0)),
            IterState::LazyCandidates { ids, .. } => (0, Some(ids.len())),
            IterState::Materialized { inner } => inner.size_hint(),
        }
    }
}

impl<G: GraphAccess + ?Sized> std::iter::FusedIterator for PatternMatchIter<'_, G> {}

// ---------------------------------------------------------------------------
// Internal IR types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Segment {
    node: NodeConstraint,
    hops: Vec<Hop>,
}

#[derive(Debug, Clone)]
struct Hop {
    edge: EdgeConstraint,
    node: NodeConstraint,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Graph;

    #[test]
    fn pattern_match_iter_is_fused_iterator() {
        fn assert_fused<I: std::iter::FusedIterator>() {}
        assert_fused::<PatternMatchIter<'_, Graph>>();
    }
}
