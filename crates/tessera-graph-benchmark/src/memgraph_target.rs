// Copyright (c) 2026 BelowZero Security OU. All rights reserved.
// SPDX-License-Identifier: LicenseRef-BelowZero-Enterprise

//! [`BenchmarkTarget`] implementation backed by Memgraph via the Bolt protocol.
//!
//! Gated behind the `memgraph` feature flag. Requires a running Memgraph
//! instance reachable at the configured URI.
//!
//! Memgraph uses `id()` (returning `i64`) instead of Neo4j 5+'s `elementId()`
//! (returning `String`), so all queries use the integer-based identity function.

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
    node_ids: HashMap<u64, i64>,
    edge_ids: HashMap<u64, i64>,
    next_handle: AtomicU64,
}

impl MemgraphTarget {
    /// Connects to a Memgraph instance.
    ///
    /// # Errors
    ///
    /// Returns [`BenchmarkError::External`] if the connection cannot be established.
    pub fn connect(uri: &str, user: &str, pass: &str, cert_path: Option<&str>) -> Result<Self> {
        let rt = Runtime::new().map_err(|e| {
            BenchmarkError::external(format!("failed to create tokio runtime: {e}"))
        })?;

        let graph = rt.block_on(async {
            let mut config = ConfigBuilder::default()
                .uri(uri)
                .user(if user.is_empty() { "neo4j" } else { user })
                .password(if pass.is_empty() { "neo4j" } else { pass })
                .db("memgraph")
                .fetch_size(500);
            if let Some(path) = cert_path {
                config = config.with_client_certificate(path);
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
        let cert = std::env::var("MEMGRAPH_CERT").ok();
        Self::connect(&uri, &user, &pass, cert.as_deref())
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
        let q = query(&format!("CREATE (n:{label}) RETURN id(n) AS nid"));
        let handle_id = self.next_handle();

        let nid: i64 = self.rt.block_on(async {
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
            row.get::<i64>("nid")
                .map_err(|e| BenchmarkError::external(format!("failed to get nid: {e}")))
        })?;

        self.node_ids.insert(handle_id, nid);
        Ok(NodeHandle(handle_id))
    }

    fn create_edge(
        &mut self,
        label: &str,
        from: NodeHandle,
        to: NodeHandle,
        _props: Properties,
    ) -> Result<EdgeHandle> {
        let from_nid = *self
            .node_ids
            .get(&from.0)
            .ok_or_else(|| BenchmarkError::external("unknown source node handle"))?;
        let to_nid = *self
            .node_ids
            .get(&to.0)
            .ok_or_else(|| BenchmarkError::external("unknown target node handle"))?;

        let q = query(&format!(
            "MATCH (a) WHERE id(a) = $from \
             MATCH (b) WHERE id(b) = $to \
             CREATE (a)-[r:{label}]->(b) \
             RETURN id(r) AS rid"
        ))
        .param("from", from_nid)
        .param("to", to_nid);

        let handle_id = self.next_handle();

        let rid: i64 = self.rt.block_on(async {
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
            row.get::<i64>("rid")
                .map_err(|e| BenchmarkError::external(format!("failed to get edge rid: {e}")))
        })?;

        self.edge_ids.insert(handle_id, rid);
        Ok(EdgeHandle(handle_id))
    }

    fn get_node(&self, handle: NodeHandle) -> Result<NodeData> {
        let nid = *self
            .node_ids
            .get(&handle.0)
            .ok_or_else(|| BenchmarkError::external("unknown node handle"))?;

        let q = query("MATCH (n) WHERE id(n) = $nid RETURN labels(n) AS lbls").param("nid", nid);

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
        let rid = *self
            .edge_ids
            .get(&handle.0)
            .ok_or_else(|| BenchmarkError::external("unknown edge handle"))?;

        let q =
            query("MATCH ()-[r]->() WHERE id(r) = $rid RETURN type(r) AS t").param("rid", rid);

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
        let start_nid = *self
            .node_ids
            .get(&start.0)
            .ok_or_else(|| BenchmarkError::external("unknown start node handle"))?;

        let q = query(&format!(
            "MATCH (s) WHERE id(s) = $nid \
             MATCH p=(s)-[*1..{max_depth}]->(n) \
             RETURN DISTINCT id(n) AS nid"
        ))
        .param("nid", start_nid);

        let nids: Vec<i64> = self.rt.block_on(async {
            let mut result = self
                .graph
                .execute(q)
                .await
                .map_err(|e| Self::bolt_err(&e))?;
            let mut nids = vec![start_nid];
            while let Some(row) = result.next().await.map_err(|e| Self::bolt_err(&e))? {
                let nid: i64 = row
                    .get("nid")
                    .map_err(|e| BenchmarkError::external(format!("bfs nid: {e}")))?;
                if !nids.contains(&nid) {
                    nids.push(nid);
                }
            }
            Ok::<_, BenchmarkError>(nids)
        })?;

        let handles: Vec<NodeHandle> = nids
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
        let from_nid = *self
            .node_ids
            .get(&from.0)
            .ok_or_else(|| BenchmarkError::external("unknown from node handle"))?;
        let to_nid = *self
            .node_ids
            .get(&to.0)
            .ok_or_else(|| BenchmarkError::external("unknown to node handle"))?;

        // Memgraph uses `[*BFS]` for shortest path traversal, not Cypher's
        // `shortestPath()` function which it does not support.
        let q = query(
            "MATCH (a) WHERE id(a) = $from \
             MATCH (b) WHERE id(b) = $to \
             MATCH p = (a)-[*BFS]->(b) \
             RETURN [n IN nodes(p) | id(n)] AS path_nids",
        )
        .param("from", from_nid)
        .param("to", to_nid);

        self.rt.block_on(async {
            let mut result = self
                .graph
                .execute(q)
                .await
                .map_err(|e| Self::bolt_err(&e))?;
            match result.next().await.map_err(|e| Self::bolt_err(&e))? {
                Some(row) => {
                    let path_nids: Vec<i64> = row
                        .get("path_nids")
                        .map_err(|e| BenchmarkError::external(format!("path nids: {e}")))?;
                    // Map graph node IDs back to NodeHandles via reverse lookup.
                    let reverse: std::collections::HashMap<i64, u64> =
                        self.node_ids.iter().map(|(&h, &nid)| (nid, h)).collect();
                    let handles: Vec<NodeHandle> = path_nids
                        .iter()
                        .filter_map(|nid| reverse.get(nid).map(|&h| NodeHandle(h)))
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
