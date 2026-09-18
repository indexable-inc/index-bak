//! Complexity metrics for any directed acyclic graph.
//!
//! Complexity is a property of the graph, not of what the graph is made of. A
//! Nix derivation closure, a Rust module graph and a CI task DAG all answer the
//! same questions: what does a change to this node invalidate, how short can
//! the whole thing get, and where is one trivial node holding everything
//! hostage. So the shapes live here once and every analyzer is an adapter that
//! supplies nodes, edges, and an optional per-node cost.
//!
//! Build a graph with [`Builder`], then call [`Dag::analyze`] for the metrics of
//! one graph or [`diff`] for the regression-gate view of two.
//!
//! ```
//! # use dag_complexity_core::{Builder, Node};
//! let mut builder = Builder::new();
//! let env = builder.node(Node::new("env-var").with_cost(0.1));
//! for unit in 0..3 {
//!     let id = builder.node(Node::new(format!("unit-{unit}")).with_cost(40.0));
//!     builder.depends_on(id, env);
//! }
//! let dag = builder.build().expect("acyclic");
//! let analysis = dag.analyze();
//! assert_eq!(analysis.ranked[0].key, "env-var");
//! assert_eq!(analysis.ranked[0].blast_radius, 3);
//! ```

mod diff;
mod graph;
mod interchange;
mod metrics;

#[cfg(test)]
mod tests;

pub use crate::diff::{Change, Diff, FrontierCause, diff};
pub use crate::graph::{Builder, BuildError, Dag, Node, NodeId};
pub use crate::interchange::{Edge, GraphFile, NodeSpec};
pub use crate::metrics::{
    Analysis, Concentration, CriticalPath, Leverage, Ranked, Share, WidthProfile,
};
