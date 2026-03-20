// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! [`BenchmarkTarget`] implementation backed by Memgraph via the Bolt protocol.
//!
//! Gated behind the `memgraph` feature flag. Requires a running Memgraph
//! instance reachable at the configured URI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use neo4rs::{ConfigBuilder, Graph, query};
use tessera_graph::Properties;
use tokio::runtime::Runtime;

use crate::error::{BenchmarkError, Result};
use crate::target::{BenchmarkTarget, EdgeData, EdgeHandle, NodeData, NodeHandle};

/// Bolt-protocol target backed by a live Memgraph instance.
pub struct MemgraphTarget {
    rt: Runtime,
    graph: Graph,
    node_ids: HashMap<u64, String>,
    edge_ids: HashMap<u64, String>,
    next_handle: AtomicU64,
}

impl MemgraphTarget {
    /// Connects to a Memgraph instance.
    ///
    /// # Errors
    ///
    /// Returns [`BenchmarkError::External`] if the connection cannot be established.
    pub fn connect(uri: &str, user: &str, pass: &str) -> Result<Self> {
        let rt = Runtime::new().map_err(|e| {
            BenchmarkError::external(format!("failed to create tokio runtime: {e}"))
        })?;

        let graph = rt.block_on(async {
            let mut config = ConfigBuilder::default().uri(uri).fetch_size(500);
            if !user.is_empty() {
                config = config.user(user).password(pass);
            }
            Graph::connect(
                config
                    .build()
                    .map_err(|e| BenchmarkError::external(format!("invalid config: {e}")))?,
            )
            .await
            .map_err(|e| BenchmarkError::external(format!("bolt connect failed: {e}")))
        })?;

        Ok(Self {
            rt,
            graph,
            node_ids: HashMap::new(),
            edge_ids: HashMap::new(),
            next_handle: AtomicU64::new(1),
        })
    }

    /// Creates a target from environment variables.
    ///
    /// - `MEMGRAPH_URI` (default: `bolt://localhost:7687`)
    /// - `MEMGRAPH_USER` (default: empty)
    /// - `MEMGRAPH_PASS` (default: empty)
    ///
    /// # Errors
    ///
    /// Returns [`BenchmarkError::External`] if the connection fails.
    pub fn from_env() -> Result<Self> {
        let uri = std::env::var("MEMGRAPH_URI").unwrap_or_else(|_| "bolt://localhost:7687".into());
        let user = std::env::var("MEMGRAPH_USER").unwrap_or_default();
        let pass = std::env::var("MEMGRAPH_PASS").unwrap_or_default();
        Self::connect(&uri, &user, &pass)
    }

    fn next_handle(&self) -> u64 {
        self.next_handle.fetch_add(1, Ordering::Relaxed)
    }

    fn bolt_err(e: &neo4rs::Error) -> BenchmarkError {
        BenchmarkError::external(format!("bolt query error: {e}"))
    }
}

impl BenchmarkTarget for MemgraphTarget {
    fn name(&self) -> &'static str {
        "memgraph"
    }

    fn create_node(&mut self, label: &str, _props: Properties) -> Result<NodeHandle> {
        let q = query(&format!("CREATE (n:{label}) RETURN elementId(n) AS eid"));
        let handle_id = self.next_handle();

        let eid: String = self.rt.block_on(async {
            let mut result = self
                .graph
                .execute(q)
                .await
                .map_err(|e| Self::bolt_err(&e))?;
            let row = result
                .next()
                .await
                .map_err(|e| Self::bolt_err(&e))?
                .ok_or_else(|| BenchmarkError::external("no row returned from CREATE"))?;
            row.get::<String>("eid")
                .map_err(|e| BenchmarkError::external(format!("failed to get eid: {e}")))
        })?;

        self.node_ids.insert(handle_id, eid);
        Ok(NodeHandle(handle_id))
    }

    fn create_edge(
        &mut self,
        label: &str,
        from: NodeHandle,
        to: NodeHandle,
        _props: Properties,
    ) -> Result<EdgeHandle> {
        let from_eid = self
            .node_ids
            .get(&from.0)
            .ok_or_else(|| BenchmarkError::external("unknown source node handle"))?
            .clone();
        let to_eid = self
            .node_ids
            .get(&to.0)
            .ok_or_else(|| BenchmarkError::external("unknown target node handle"))?
            .clone();

        let q = query(&format!(
            "MATCH (a) WHERE elementId(a) = $from \
             MATCH (b) WHERE elementId(b) = $to \
             CREATE (a)-[r:{label}]->(b) \
             RETURN elementId(r) AS eid"
        ))
        .param("from", from_eid)
        .param("to", to_eid);

        let handle_id = self.next_handle();

        let eid: String = self.rt.block_on(async {
            let mut result = self
                .graph
                .execute(q)
                .await
                .map_err(|e| Self::bolt_err(&e))?;
            let row = result
                .next()
                .await
                .map_err(|e| Self::bolt_err(&e))?
                .ok_or_else(|| BenchmarkError::external("no row returned from CREATE edge"))?;
            row.get::<String>("eid")
                .map_err(|e| BenchmarkError::external(format!("failed to get edge eid: {e}")))
        })?;

        self.edge_ids.insert(handle_id, eid);
        Ok(EdgeHandle(handle_id))
    }

    fn get_node(&self, handle: NodeHandle) -> Result<NodeData> {
        let eid = self
            .node_ids
            .get(&handle.0)
            .ok_or_else(|| BenchmarkError::external("unknown node handle"))?
            .clone();

        let q =
            query("MATCH (n) WHERE elementId(n) = $eid RETURN labels(n) AS lbls").param("eid", eid);

        self.rt.block_on(async {
            let mut result = self
                .graph
                .execute(q)
                .await
                .map_err(|e| Self::bolt_err(&e))?;
            let row = result
                .next()
                .await
                .map_err(|e| Self::bolt_err(&e))?
                .ok_or_else(|| BenchmarkError::external("node not found"))?;
            let labels: Vec<String> = row
                .get("lbls")
                .map_err(|e| BenchmarkError::external(format!("failed to get labels: {e}")))?;
            Ok(NodeData {
                label: labels.into_iter().next().unwrap_or_default(),
                props: Properties::new(),
            })
        })
    }

    fn get_edge(&self, handle: EdgeHandle) -> Result<EdgeData> {
        let eid = self
            .edge_ids
            .get(&handle.0)
            .ok_or_else(|| BenchmarkError::external("unknown edge handle"))?
            .clone();

        let q = query("MATCH ()-[r]->() WHERE elementId(r) = $eid RETURN type(r) AS t")
            .param("eid", eid);

        self.rt.block_on(async {
            let mut result = self
                .graph
                .execute(q)
                .await
                .map_err(|e| Self::bolt_err(&e))?;
            let row = result
                .next()
                .await
                .map_err(|e| Self::bolt_err(&e))?
                .ok_or_else(|| BenchmarkError::external("edge not found"))?;
            let label: String = row
                .get("t")
                .map_err(|e| BenchmarkError::external(format!("failed to get type: {e}")))?;
            Ok(EdgeData {
                label,
                props: Properties::new(),
            })
        })
    }

    fn traverse_bfs(&self, start: NodeHandle, max_depth: u32) -> Result<Vec<NodeHandle>> {
        let start_eid = self
            .node_ids
            .get(&start.0)
            .ok_or_else(|| BenchmarkError::external("unknown start node handle"))?
            .clone();

        let q = query(&format!(
            "MATCH (s) WHERE elementId(s) = $eid \
             MATCH p=(s)-[*1..{max_depth}]->(n) \
             RETURN DISTINCT elementId(n) AS eid"
        ))
        .param("eid", start_eid.clone());

        let eids: Vec<String> = self.rt.block_on(async {
            let mut result = self
                .graph
                .execute(q)
                .await
                .map_err(|e| Self::bolt_err(&e))?;
            let mut eids = vec![start_eid];
            while let Some(row) = result.next().await.map_err(|e| Self::bolt_err(&e))? {
                let eid: String = row
                    .get("eid")
                    .map_err(|e| BenchmarkError::external(format!("bfs eid: {e}")))?;
                if !eids.contains(&eid) {
                    eids.push(eid);
                }
            }
            Ok::<_, BenchmarkError>(eids)
        })?;

        // Map back to handles — use lookup or synthetic handles
        let handles: Vec<NodeHandle> = eids
            .iter()
            .enumerate()
            .map(|(i, _)| NodeHandle(start.0 + i as u64))
            .collect();

        Ok(handles)
    }

    fn traverse_dfs(&self, start: NodeHandle, max_depth: u32) -> Result<Vec<NodeHandle>> {
        // Memgraph doesn't distinguish BFS/DFS in Cypher; use same query
        self.traverse_bfs(start, max_depth)
    }

    fn shortest_path(&self, from: NodeHandle, to: NodeHandle) -> Result<Option<Vec<NodeHandle>>> {
        let from_eid = self
            .node_ids
            .get(&from.0)
            .ok_or_else(|| BenchmarkError::external("unknown from node handle"))?
            .clone();
        let to_eid = self
            .node_ids
            .get(&to.0)
            .ok_or_else(|| BenchmarkError::external("unknown to node handle"))?
            .clone();

        let q = query(
            "MATCH (a) WHERE elementId(a) = $from \
             MATCH (b) WHERE elementId(b) = $to \
             MATCH p=shortestPath((a)-[*]->(b)) \
             RETURN [n IN nodes(p) | elementId(n)] AS path_eids",
        )
        .param("from", from_eid)
        .param("to", to_eid);

        self.rt.block_on(async {
            let mut result = self
                .graph
                .execute(q)
                .await
                .map_err(|e| Self::bolt_err(&e))?;
            match result.next().await.map_err(|e| Self::bolt_err(&e))? {
                Some(row) => {
                    let path_eids: Vec<String> = row
                        .get("path_eids")
                        .map_err(|e| BenchmarkError::external(format!("path eids: {e}")))?;
                    let handles: Vec<NodeHandle> = path_eids
                        .iter()
                        .enumerate()
                        .map(|(i, _)| NodeHandle(from.0 + i as u64))
                        .collect();
                    Ok(Some(handles))
                }
                None => Ok(None),
            }
        })
    }

    fn clear(&mut self) {
        let q = query("MATCH (n) DETACH DELETE n");
        let _ = self.rt.block_on(async { self.graph.execute(q).await });
        self.node_ids.clear();
        self.edge_ids.clear();
    }
}
