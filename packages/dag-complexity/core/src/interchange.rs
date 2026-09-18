//! The JSON graph an out-of-tree adapter can emit.
//!
//! An adapter written in this workspace should depend on the crate and call
//! [`crate::Builder`] directly. This exists for the ones that cannot: a shell
//! pipeline, a Python script, a tool in another repo. Same metrics, same diff,
//! one file format:
//!
//! ```json
//! {
//!   "nodes": [{"key": "a", "label": "a", "kind": "unit", "cost": 12.5, "version": "sha"}],
//!   "edges": [{"dependent": "a", "dependency": "b"}]
//! }
//! ```
//!
//! `dependent` needs `dependency`, so a change to `dependency` invalidates
//! `dependent`. Naming both ends beats a `from`/`to` whose direction a reader
//! has to guess.

use serde::{Deserialize, Serialize};

use crate::graph::{BuildError, Builder, Dag, Node};

/// One node in the interchange format. Mirrors [`Node`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// See [`Node::members`]. Written by an export, ignored on import: an
    /// adapter declares real nodes and lets the core condense them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

/// One edge: `dependent` must be rebuilt when `dependency` changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub dependent: String,
    pub dependency: String,
}

/// A whole graph, as read from or written to JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphFile {
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<Edge>,
}

impl GraphFile {
    /// Turn the file into a [`Dag`].
    ///
    /// An edge naming a key with no node declares that node implicitly, with no
    /// cost: an adapter that lists only the interesting nodes still gets a
    /// connected graph rather than a silently truncated one.
    ///
    /// # Errors
    ///
    /// [`BuildError::DuplicateKey`] if two entries share a key, or
    /// [`BuildError::Cycle`] if the edges are not acyclic.
    pub fn into_dag(self) -> Result<Dag, BuildError> {
        self.build(false)
    }

    /// As [`GraphFile::into_dag`], but collapsing reference cycles into single
    /// nodes rather than refusing them. See [`Builder::build_condensed`].
    ///
    /// # Errors
    ///
    /// [`BuildError::DuplicateKey`] if two entries share a key.
    pub fn into_condensed_dag(self) -> Result<Dag, BuildError> {
        self.build(true)
    }

    fn build(self, condense: bool) -> Result<Dag, BuildError> {
        let mut builder = Builder::new();
        for spec in self.nodes {
            let mut node = Node::new(spec.key);
            if let Some(label) = spec.label {
                node = node.with_label(label);
            }
            if let Some(kind) = spec.kind {
                node = node.with_kind(kind);
            }
            if let Some(cost) = spec.cost {
                node = node.with_cost(cost);
            }
            if let Some(version) = spec.version {
                node = node.with_version(version);
            }
            builder.node(node);
        }
        for edge in self.edges {
            let dependent = builder.node_for(&edge.dependent);
            let dependency = builder.node_for(&edge.dependency);
            builder.depends_on(dependent, dependency);
        }
        if condense {
            builder.build_condensed()
        } else {
            builder.build()
        }
    }
}

impl From<&Dag> for GraphFile {
    fn from(dag: &Dag) -> Self {
        Self {
            nodes: dag
                .nodes()
                .map(|node| NodeSpec {
                    key: node.key.clone(),
                    label: Some(node.label.clone()),
                    kind: node.kind.clone(),
                    cost: node.cost,
                    version: node.version.clone(),
                    members: node.members.clone(),
                })
                .collect(),
            edges: (0..dag.len())
                .flat_map(|index| {
                    let id = crate::graph::NodeId(index);
                    dag.dependencies(id).iter().map(move |dependency| Edge {
                        dependent: dag.node(id).key.clone(),
                        dependency: dag.node(*dependency).key.clone(),
                    })
                })
                .collect(),
        }
    }
}
