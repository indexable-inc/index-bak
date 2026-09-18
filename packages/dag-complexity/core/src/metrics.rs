//! The metrics themselves. Every one of them answers a question an operator
//! actually asks; anything that only produced a number nobody could argue with
//! was left out (see the crate README).

use serde::Serialize;

use crate::graph::{Dag, NodeId};

/// Everything one graph has to say about itself.
#[derive(Debug, Clone, Serialize)]
pub struct Analysis {
    pub nodes: usize,
    pub edges: usize,
    /// Nodes nothing depends on: the things the graph was asked to produce.
    pub roots: usize,
    /// Nodes that depend on nothing: where the work starts.
    pub leaves: usize,
    /// Every node, worst blast radius first.
    pub ranked: Vec<Ranked>,
    pub critical_path: CriticalPath,
    pub width: WidthProfile,
    pub concentration: Concentration,
    /// Cheap nodes with large blast radius, worst first. Empty when no node
    /// carries a cost; `leverage_costed` says how many nodes qualified.
    pub leverage: Vec<Leverage>,
    /// How many nodes carried a cost, and so could be ranked by leverage.
    pub leverage_costed: usize,
}

/// One node's standing in the graph.
#[derive(Debug, Clone, Serialize)]
pub struct Ranked {
    pub key: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// Nodes that transitively depend on this one: everything a change here
    /// invalidates. The headline number.
    pub blast_radius: usize,
    /// Direct dependents. Together with `blast_radius` this separates "one node
    /// feeds everything" (fan-out near blast radius) from "long chain"
    /// (fan-out of one, blast radius of hundreds).
    pub fan_out: usize,
    /// Longest chain of dependencies below this node, in nodes. The earliest
    /// level at which it could start.
    pub depth: usize,
    /// How many original nodes this one stands for. Above one when a reference
    /// cycle was condensed into it; the members are in `Node::members`.
    pub merged: usize,
}

/// The floor on wall clock: no amount of parallelism beats the longest chain.
#[derive(Debug, Clone, Serialize)]
pub struct CriticalPath {
    /// Length of `path` in nodes.
    pub nodes: usize,
    /// The longest chain by node count, which is not always the heaviest one by
    /// cost. Reported separately because it is the floor on *steps*, and a
    /// scheduler that cannot overlap steps is bound by this rather than by
    /// cost. Equals the number of levels in the width profile.
    pub longest_chain: usize,
    /// Length in cost, when every node on the path carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// Sum of every node's cost: what one worker would spend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
    /// `total_cost / cost`: the speedup an unlimited number of workers would
    /// reach, and the ceiling on any scheduling change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ideal_speedup: Option<f64>,
    /// The chain itself, dependencies first.
    pub path: Vec<String>,
}

/// Available parallelism, level by level. A level is the earliest point a node
/// could start given its dependencies, so the count at each level is how many
/// workers could be busy there.
#[derive(Debug, Clone, Serialize)]
pub struct WidthProfile {
    pub levels: usize,
    pub max: usize,
    pub mean: f64,
    /// Nodes per level, level 0 first.
    pub histogram: Vec<usize>,
}

/// How much of the graph's total fragility sits in how few nodes.
#[derive(Debug, Clone, Serialize)]
pub struct Concentration {
    /// Summed blast radius over every node. The denominator for each share.
    pub total_blast_radius: u64,
    pub top: Vec<Share>,
}

/// The share of total blast radius held by the worst `nodes` nodes.
#[derive(Debug, Clone, Serialize)]
pub struct Share {
    pub label: String,
    pub nodes: usize,
    pub share: f64,
}

/// A node that costs little and breaks much. A large expensive node with many
/// dependents is a compiler and is fine; a trivial one is a defect.
#[derive(Debug, Clone, Serialize)]
pub struct Leverage {
    pub key: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `blast_share * (1 - cost_percentile)`, in `0.0..=1.0`.
    pub score: f64,
    pub blast_radius: usize,
    /// `blast_radius` over every other node in the graph.
    pub blast_share: f64,
    pub cost: f64,
    /// Fraction of costed nodes that cost strictly less than this one.
    pub cost_percentile: f64,
}

impl Leverage {
    /// The one-line justification, so a reader can disagree with the ranking
    /// rather than take it on faith.
    #[must_use]
    pub fn explain(&self) -> String {
        format!(
            "cost {:.3} is below {:.0}% of the graph, yet {} nodes ({:.0}%) rebuild when it changes",
            self.cost,
            (1.0 - self.cost_percentile) * 100.0,
            self.blast_radius,
            self.blast_share * 100.0,
        )
    }
}

/// A bitset over node ids, one `u64` word per 64 nodes.
struct Reach {
    words: usize,
    bits: Vec<u64>,
}

impl Reach {
    fn new(count: usize) -> Self {
        let words = count.div_ceil(64);
        Self {
            words,
            bits: vec![0; words * count],
        }
    }

    fn set(&mut self, owner: usize, member: usize) {
        self.bits[owner * self.words + member / 64] |= 1 << (member % 64);
    }

    /// `dest |= src`, for two distinct rows.
    fn union_into(&mut self, dest: usize, src: usize) {
        for word in 0..self.words {
            let value = self.bits[src * self.words + word];
            self.bits[dest * self.words + word] |= value;
        }
    }

    fn count(&self, owner: usize) -> usize {
        self.bits[owner * self.words..(owner + 1) * self.words]
            .iter()
            .map(|word| usize::try_from(word.count_ones()).unwrap_or(usize::MAX))
            .sum()
    }
}

/// Transitive dependents of every node, exactly.
///
/// One bitset row per node, filled in reverse topological order so a node's
/// dependents are already complete when it is reached. Exact rather than
/// estimated because the number is the headline and an approximation nobody
/// can check is worse than no number. The cost is `nodes^2 / 8` bytes, which
/// is the operating envelope: fine into the tens of thousands of nodes.
fn transitive_dependents(dag: &Dag) -> Vec<usize> {
    let count = dag.len();
    let mut reach = Reach::new(count);
    for id in dag.order.iter().rev() {
        for dependent in dag.dependents(*id) {
            reach.set(id.0, dependent.0);
            reach.union_into(id.0, dependent.0);
        }
    }
    (0..count).map(|id| reach.count(id)).collect()
}

/// Longest-path depth of every node in nodes, and the node ending the longest
/// chain overall.
struct Depths {
    depth: Vec<usize>,
    deepest: Option<NodeId>,
}

fn depths(dag: &Dag) -> Depths {
    let mut depth = vec![0usize; dag.len()];
    let mut deepest = None;
    let mut best = 0;
    for id in &dag.order {
        let level = dag
            .dependencies(*id)
            .iter()
            .map(|dependency| depth[dependency.0] + 1)
            .max()
            .unwrap_or(0);
        depth[id.0] = level;
        if deepest.is_none() || level > best {
            best = level;
            deepest = Some(*id);
        }
    }
    Depths { depth, deepest }
}

/// The heaviest chain by cost, and the node it ends at. `None` when any node
/// lacks a cost: a critical path computed over a graph where some nodes count
/// as free is a number that reads as a floor while being below the real one.
struct CostPath {
    cost: Vec<f64>,
    heaviest: Option<NodeId>,
}

fn cost_path(dag: &Dag) -> Option<CostPath> {
    let mut cost = vec![0.0f64; dag.len()];
    let mut heaviest = None;
    let mut best = f64::NEG_INFINITY;
    for id in &dag.order {
        let own = dag.node(*id).cost?;
        let below = dag
            .dependencies(*id)
            .iter()
            .map(|dependency| cost[dependency.0])
            .fold(0.0f64, f64::max);
        cost[id.0] = own + below;
        if cost[id.0] > best {
            best = cost[id.0];
            heaviest = Some(*id);
        }
    }
    Some(CostPath { cost, heaviest })
}

/// Walk back from `end` along the dependency that carries the chain, using
/// `score` as the tiebreak-free choice of predecessor.
fn reconstruct(dag: &Dag, end: NodeId, score: &[f64]) -> Vec<String> {
    let mut path = vec![dag.node(end).key.clone()];
    let mut current = end;
    while let Some(next) = dag
        .dependencies(current)
        .iter()
        .copied()
        .max_by(|left, right| {
            score[left.0]
                .partial_cmp(&score[right.0])
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    {
        path.push(dag.node(next).key.clone());
        current = next;
    }
    path.reverse();
    path
}

#[expect(
    clippy::cast_precision_loss,
    reason = "node counts are exact in f64 far past any graph this runs on"
)]
fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

impl Dag {
    /// Every metric, computed in one pass over the graph.
    #[must_use]
    pub fn analyze(&self) -> Analysis {
        let blast = transitive_dependents(self);
        let levels = depths(self);
        let costs = cost_path(self);

        let mut ranked: Vec<Ranked> = (0..self.len())
            .map(|index| {
                let node = self.node(NodeId(index));
                Ranked {
                    key: node.key.clone(),
                    label: node.label.clone(),
                    kind: node.kind.clone(),
                    cost: node.cost,
                    blast_radius: blast[index],
                    fan_out: self.dependents(NodeId(index)).len(),
                    depth: levels.depth[index],
                    merged: node.weight(),
                }
            })
            .collect();
        ranked.sort_by(|left, right| {
            right
                .blast_radius
                .cmp(&left.blast_radius)
                .then(right.fan_out.cmp(&left.fan_out))
                .then(left.key.cmp(&right.key))
        });

        Analysis {
            nodes: self.len(),
            edges: self.edge_count(),
            roots: (0..self.len())
                .filter(|index| self.dependents(NodeId(*index)).is_empty())
                .count(),
            leaves: (0..self.len())
                .filter(|index| self.dependencies(NodeId(*index)).is_empty())
                .count(),
            critical_path: self.critical_path(&levels, costs.as_ref()),
            width: width_profile(&levels.depth),
            concentration: concentration(&blast),
            leverage: leverage(self, &blast),
            leverage_costed: self.nodes().filter(|node| node.cost.is_some()).count(),
            ranked,
        }
    }

    fn critical_path(&self, levels: &Depths, costs: Option<&CostPath>) -> CriticalPath {
        let total_cost = self
            .nodes()
            .map(|node| node.cost)
            .try_fold(0.0f64, |sum, cost| cost.map(|cost| sum + cost));
        let (cost, path) = match (costs, costs.and_then(|costs| costs.heaviest)) {
            (Some(costs), Some(end)) => (
                Some(costs.cost[end.0]),
                reconstruct(self, end, &costs.cost),
            ),
            _ => (
                None,
                levels.deepest.map_or_else(Vec::new, |end| {
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "depths are small integers, exact in f64"
                    )]
                    let score: Vec<f64> =
                        levels.depth.iter().map(|depth| *depth as f64).collect();
                    reconstruct(self, end, &score)
                }),
            ),
        };
        CriticalPath {
            nodes: path.len(),
            longest_chain: levels.depth.iter().copied().max().map_or(0, |max| max + 1),
            cost,
            total_cost,
            ideal_speedup: match (total_cost, cost) {
                (Some(total), Some(critical)) if critical > 0.0 => Some(total / critical),
                _ => None,
            },
            path,
        }
    }
}

fn width_profile(depth: &[usize]) -> WidthProfile {
    let levels = depth.iter().copied().max().map_or(0, |max| max + 1);
    let mut histogram = vec![0usize; levels];
    for level in depth {
        histogram[*level] += 1;
    }
    WidthProfile {
        levels,
        max: histogram.iter().copied().max().unwrap_or(0),
        mean: ratio(depth.len(), levels),
        histogram,
    }
}

fn concentration(blast: &[usize]) -> Concentration {
    let mut sorted: Vec<usize> = blast.to_vec();
    sorted.sort_unstable_by(|left, right| right.cmp(left));
    let total: u64 = sorted.iter().map(|value| *value as u64).sum();
    let share = |take: usize| -> Share {
        let take = take.min(sorted.len());
        let held: u64 = sorted.iter().take(take).map(|value| *value as u64).sum();
        Share {
            label: format!("top {take}"),
            nodes: take,
            #[expect(
                clippy::cast_precision_loss,
                reason = "summed blast radii stay far below 2^53"
            )]
            share: if total == 0 {
                0.0
            } else {
                held as f64 / total as f64
            },
        }
    };
    Concentration {
        total_blast_radius: total,
        top: vec![share(1), share(10), share(sorted.len().div_ceil(100))],
    }
}

/// Rank nodes by cheapness times blast radius.
///
/// `score = blast_share * (1 - cost_percentile)`. A compiler scores near zero
/// because its cost percentile is near one; a node that does nothing and is
/// depended on by everything scores near one. Nodes without a cost are left
/// out rather than assumed free, which would put every unmeasured node at the
/// top of the list that exists to find defects.
fn leverage(dag: &Dag, blast: &[usize]) -> Vec<Leverage> {
    let mut sorted_costs: Vec<f64> = dag.nodes().filter_map(|node| node.cost).collect();
    if sorted_costs.is_empty() {
        return Vec::new();
    }
    sorted_costs.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let costed = sorted_costs.len();
    let others = dag.len().saturating_sub(1);

    let mut ranked: Vec<Leverage> = (0..dag.len())
        .filter_map(|index| {
            let node = dag.node(NodeId(index));
            let cost = node.cost?;
            let below = sorted_costs.partition_point(|value| *value < cost);
            let cost_percentile = ratio(below, costed);
            let blast_share = ratio(blast[index], others);
            Some(Leverage {
                key: node.key.clone(),
                label: node.label.clone(),
                kind: node.kind.clone(),
                score: blast_share * (1.0 - cost_percentile),
                blast_radius: blast[index],
                blast_share,
                cost,
                cost_percentile,
            })
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.key.cmp(&right.key))
    });
    ranked
}
