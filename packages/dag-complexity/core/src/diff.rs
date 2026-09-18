//! Two graphs in, "what changed and what did each change invalidate" out.
//!
//! This is the part that turns a report into a regression gate. Every adapter
//! gets it for free: the only thing that varies is what an adapter calls a
//! node's content identity ([`crate::Node::version`], or the key itself when
//! the key is already content-addressed, as a `.drv` basename is).

use std::collections::{BTreeSet, HashSet, VecDeque};

use serde::Serialize;

use crate::graph::{Dag, NodeId};

/// How a node differs between the two graphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    /// Present only in the later graph.
    Added,
    /// Present only in the earlier graph.
    Removed,
    /// Same key, different [`crate::Node::version`].
    Modified,
}

/// A node that changed on its own, plus what it took down with it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FrontierCause {
    pub key: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    pub change: Change,
    /// Transitive dependents in the later graph, restricted to the targets
    /// when `diff` was given some. Sorted, so two runs compare cleanly.
    pub dependents: Vec<String>,
}

/// What moved between two graphs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diff {
    pub before_nodes: usize,
    pub after_nodes: usize,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified: Vec<String>,
    /// Changed nodes plus everything transitively above them in the later
    /// graph: the work the change costs.
    pub invalidated: usize,
    /// `invalidated` over the later graph's node count.
    pub invalidated_share: f64,
    /// Changed nodes whose own dependencies all held still. These are the real
    /// causes; everything else on the `invalidated` list is propagation.
    /// Ranked by how many dependents each explains.
    pub frontier: Vec<FrontierCause>,
}

/// Compare two graphs.
///
/// A node is `Modified` when its key appears in both graphs with a different
/// [`crate::Node::version`]. An adapter whose keys are already
/// content-addressed leaves `version` unset and gets `Added`/`Removed`
/// instead, which carries the same information.
///
/// `targets` narrows the dependent lists to the nodes the caller actually
/// asked the graph to produce (a CI check set, a set of binaries). Frontier
/// causes that reach none of them drop out. Pass `None` for the whole graph.
#[must_use]
pub fn diff(before: &Dag, after: &Dag, targets: Option<&[String]>) -> Diff {
    let before_versions: std::collections::HashMap<&str, Option<&str>> = before
        .nodes()
        .map(|node| (node.key.as_str(), node.version.as_deref()))
        .collect();
    let after_keys: HashSet<&str> = after.nodes().map(|node| node.key.as_str()).collect();

    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut changed: Vec<NodeId> = Vec::new();
    for (index, node) in after.nodes().enumerate() {
        match before_versions.get(node.key.as_str()) {
            None => {
                added.push(node.key.clone());
                changed.push(NodeId(index));
            }
            Some(version) if *version != node.version.as_deref() => {
                modified.push(node.key.clone());
                changed.push(NodeId(index));
            }
            Some(_) => {}
        }
    }
    let mut removed: Vec<String> = before
        .nodes()
        .filter(|node| !after_keys.contains(node.key.as_str()))
        .map(|node| node.key.clone())
        .collect();
    added.sort();
    modified.sort();
    removed.sort();

    let changed_set: HashSet<NodeId> = changed.iter().copied().collect();
    let target_ids = targets.map(|targets| {
        targets
            .iter()
            .filter_map(|key| after.id(key))
            .collect::<HashSet<NodeId>>()
    });

    let invalidated = reachable_upward(after, &changed).len();
    let mut frontier: Vec<FrontierCause> = changed
        .iter()
        .filter(|id| {
            !after
                .dependencies(**id)
                .iter()
                .any(|dependency| changed_set.contains(dependency))
        })
        .filter_map(|id| {
            let reached = reachable_upward(after, std::slice::from_ref(id));
            // With targets, a cause is credited with the targets it reaches,
            // itself included: a check whose own derivation moved while every
            // input held still is its own cause. Without targets the list is
            // the plain blast radius, which excludes the node itself.
            let named: Vec<String> = reached
                .iter()
                .filter(|reached| {
                    target_ids.as_ref().map_or_else(
                        || **reached != *id,
                        |targets| targets.contains(reached),
                    )
                })
                .map(|reached| after.node(*reached).key.clone())
                .collect::<BTreeSet<String>>()
                .into_iter()
                .collect();
            if target_ids.is_some() && named.is_empty() {
                return None;
            }
            let node = after.node(*id);
            Some(FrontierCause {
                key: node.key.clone(),
                label: node.label.clone(),
                kind: node.kind.clone(),
                cost: node.cost,
                change: if before_versions.contains_key(node.key.as_str()) {
                    Change::Modified
                } else {
                    Change::Added
                },
                dependents: named,
            })
        })
        .collect();
    frontier.sort_by(|left, right| {
        right
            .dependents
            .len()
            .cmp(&left.dependents.len())
            .then(left.key.cmp(&right.key))
    });

    Diff {
        before_nodes: before.len(),
        after_nodes: after.len(),
        added,
        removed,
        modified,
        invalidated,
        invalidated_share: share(invalidated, after.len()),
        frontier,
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "node counts are exact in f64 far past any graph this runs on"
)]
fn share(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

/// Every node reachable by following dependent edges from `sources`, sources
/// included.
fn reachable_upward(dag: &Dag, sources: &[NodeId]) -> HashSet<NodeId> {
    let mut seen: HashSet<NodeId> = sources.iter().copied().collect();
    let mut queue: VecDeque<NodeId> = sources.iter().copied().collect();
    while let Some(id) = queue.pop_front() {
        for dependent in dag.dependents(id) {
            if seen.insert(*dependent) {
                queue.push_back(*dependent);
            }
        }
    }
    seen
}
