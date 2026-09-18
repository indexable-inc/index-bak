//! The graph an adapter hands to the metrics: nodes with an identity, an
//! optional cost, and dependency edges.

use std::collections::HashMap;

use serde::Serialize;

/// One node. `key` is the identity an adapter guarantees is unique and stable
/// across two runs, so a diff can say "this same node changed" rather than
/// "one vanished and another appeared".
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Unique, stable identity.
    pub key: String,
    /// What a human should read. Defaults to `key`.
    pub label: String,
    /// Adapter-defined category (`crate`, `module`, `derivation`), carried
    /// through to the report so a reader can tell node kinds apart.
    pub kind: Option<String>,
    /// What this node itself costs, in whatever unit the adapter measures:
    /// build seconds, lines of code, bytes. Only the ordering matters, so
    /// adapters must not mix units within one graph.
    pub cost: Option<f64>,
    /// Content identity. Two runs whose `key` matches but whose `version`
    /// differs are a change; [`crate::diff`] needs this and nothing else does.
    pub version: Option<String>,
    /// The keys this node stands for when a cycle was condensed into it
    /// ([`Builder::build_condensed`]). Empty when the node is only itself.
    pub members: Vec<String>,
}

impl Node {
    /// A node with no cost and no version.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        let key = key.into();
        Self {
            label: key.clone(),
            key,
            kind: None,
            cost: None,
            version: None,
            members: Vec::new(),
        }
    }

    /// Override the human-readable label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Tag the node with an adapter-defined category.
    #[must_use]
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// Attach the node's own cost. See [`Node::cost`].
    #[must_use]
    pub const fn with_cost(mut self, cost: f64) -> Self {
        self.cost = Some(cost);
        self
    }

    /// Attach the node's content identity. See [`Node::version`].
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// How many original nodes this one stands for: one, unless a cycle was
    /// condensed into it.
    #[must_use]
    pub fn weight(&self) -> usize {
        self.members.len().max(1)
    }
}

/// An index into a [`Dag`]'s node table, handed out by [`Builder::node`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct NodeId(pub(crate) usize);

/// What went wrong turning a [`Builder`] into a [`Dag`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// Two nodes claimed the same `key`. Reported rather than merged: an
    /// adapter that collides its own identities produces a graph whose blast
    /// radii are silently wrong, and the caller is the only one who can say
    /// which key was meant.
    DuplicateKey(String),
    /// The edges contain a cycle. The names on one cycle, in order.
    Cycle(Vec<String>),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateKey(key) => write!(formatter, "two nodes share the key {key}"),
            Self::Cycle(keys) => write!(formatter, "the graph has a cycle: {}", keys.join(" -> ")),
        }
    }
}

impl std::error::Error for BuildError {}

/// Accumulates nodes and edges, then validates them into a [`Dag`].
#[derive(Debug, Default)]
pub struct Builder {
    nodes: Vec<Node>,
    by_key: HashMap<String, NodeId>,
    /// `(dependent, dependency)` pairs, deduplicated at build time.
    edges: Vec<(NodeId, NodeId)>,
    duplicate: Option<String>,
}

impl Builder {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node and return its id. A repeated key is remembered and reported
    /// by [`Builder::build`]; the id returned is the first node's, so callers
    /// that legitimately re-declare a node (an adapter meeting the same
    /// dependency twice) can use [`Builder::lookup`] instead.
    pub fn node(&mut self, node: Node) -> NodeId {
        if let Some(existing) = self.by_key.get(&node.key) {
            self.duplicate.get_or_insert_with(|| node.key.clone());
            return *existing;
        }
        let id = NodeId(self.nodes.len());
        self.by_key.insert(node.key.clone(), id);
        self.nodes.push(node);
        id
    }

    /// The id already assigned to `key`, if any.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<NodeId> {
        self.by_key.get(key).copied()
    }

    /// The id for `key`, adding a bare node if it is new. For adapters whose
    /// edge list names nodes it has not reached yet.
    pub fn node_for(&mut self, key: &str) -> NodeId {
        self.by_key
            .get(key)
            .copied()
            .unwrap_or_else(|| self.node(Node::new(key)))
    }

    /// Record that `dependent` must be rebuilt when `dependency` changes.
    pub fn depends_on(&mut self, dependent: NodeId, dependency: NodeId) {
        self.edges.push((dependent, dependency));
    }

    /// Validate into a [`Dag`], collapsing every reference cycle into one node
    /// first.
    ///
    /// For graph families that are cyclic by nature. A Rust module that uses a
    /// type from a module that uses one of its own is not a build ordering
    /// problem, but it is one unit of invalidation: touch either file and both
    /// are recompiled. Condensing says exactly that, and the condensed node
    /// keeps its members' keys ([`Node::members`]) so the report can name them.
    /// Its cost is the sum of theirs; its version is theirs concatenated in
    /// sorted order, so it changes when any member does.
    ///
    /// A graph that is already acyclic comes through untouched.
    ///
    /// # Errors
    ///
    /// [`BuildError::DuplicateKey`] if two nodes shared a key. Never
    /// [`BuildError::Cycle`].
    pub fn build_condensed(self) -> Result<Dag, BuildError> {
        if let Some(key) = &self.duplicate {
            return Err(BuildError::DuplicateKey(key.clone()));
        }
        let components = strongly_connected(&self.nodes, &self.edges);
        if components.iter().all(|component| component.len() == 1) {
            return self.build();
        }

        let mut condensed = Self::new();
        let mut representative = vec![NodeId(0); self.nodes.len()];
        for component in &components {
            let mut keys: Vec<&str> = component
                .iter()
                .map(|id| self.nodes[id.0].key.as_str())
                .collect();
            keys.sort_unstable();
            let first = component[0];
            let node = if component.len() == 1 {
                self.nodes[first.0].clone()
            } else {
                merge(&self.nodes, component, &keys)
            };
            let id = condensed.node(node);
            for member in component {
                representative[member.0] = id;
            }
        }
        for (dependent, dependency) in &self.edges {
            condensed.depends_on(representative[dependent.0], representative[dependency.0]);
        }
        condensed.build()
    }

    /// Validate into a [`Dag`].
    ///
    /// # Errors
    ///
    /// [`BuildError::DuplicateKey`] if two nodes shared a key, or
    /// [`BuildError::Cycle`] if the edges are not acyclic.
    pub fn build(self) -> Result<Dag, BuildError> {
        if let Some(key) = self.duplicate {
            return Err(BuildError::DuplicateKey(key));
        }
        let count = self.nodes.len();
        let mut dependencies = vec![Vec::new(); count];
        let mut dependents = vec![Vec::new(); count];
        let mut seen = std::collections::HashSet::new();
        for (dependent, dependency) in self.edges {
            // A self-edge is never information: it cannot change what any
            // metric reports, and every traversal below would have to special
            // case it. Drop it here so nothing downstream must.
            if dependent == dependency || !seen.insert((dependent, dependency)) {
                continue;
            }
            dependencies[dependent.0].push(dependency);
            dependents[dependency.0].push(dependent);
        }
        let order = topological_order(&dependencies, &dependents, &self.nodes)?;
        Ok(Dag {
            nodes: self.nodes,
            by_key: self.by_key,
            dependencies,
            dependents,
            order,
        })
    }
}

/// The condensed node for one cycle. Its key is the alphabetically first
/// member, so two runs over the same graph name it the same way.
fn merge(nodes: &[Node], component: &[NodeId], keys: &[&str]) -> Node {
    let mut versions: Vec<&str> = component
        .iter()
        .filter_map(|id| nodes[id.0].version.as_deref())
        .collect();
    versions.sort_unstable();
    let cost = component
        .iter()
        .map(|id| nodes[id.0].cost)
        .try_fold(0.0f64, |sum, cost| cost.map(|cost| sum + cost));
    let mut node = Node::new(keys[0].to_owned()).with_label(format!(
        "{} (+{} in a reference cycle)",
        keys[0],
        keys.len() - 1
    ));
    node.kind = component
        .iter()
        .find(|id| nodes[id.0].key == keys[0])
        .and_then(|id| nodes[id.0].kind.clone());
    node.cost = cost;
    node.version = (!versions.is_empty()).then(|| versions.concat());
    node.members = keys.iter().map(|key| (*key).to_owned()).collect();
    node
}

/// Tarjan's strongly connected components, iterative so a deep graph cannot
/// blow the stack. Returns one vector per component.
fn strongly_connected(nodes: &[Node], edges: &[(NodeId, NodeId)]) -> Vec<Vec<NodeId>> {
    /// Tarjan's "no discovery index yet" marker.
    const UNVISITED: usize = usize::MAX;

    let count = nodes.len();
    let mut out_edges: Vec<Vec<NodeId>> = vec![Vec::new(); count];
    for (dependent, dependency) in edges {
        out_edges[dependent.0].push(*dependency);
    }

    let mut index = vec![UNVISITED; count];
    let mut low = vec![0usize; count];
    let mut on_stack = vec![false; count];
    let mut stack: Vec<usize> = Vec::new();
    let mut next = 0usize;
    let mut components = Vec::new();
    // (node, how many of its out-edges are already walked)
    let mut work: Vec<(usize, usize)> = Vec::new();

    for root in 0..count {
        if index[root] != UNVISITED {
            continue;
        }
        work.push((root, 0));
        while let Some((node, edge)) = work.pop() {
            if edge == 0 {
                index[node] = next;
                low[node] = next;
                next += 1;
                stack.push(node);
                on_stack[node] = true;
            }
            let mut recursed = false;
            for (offset, target) in out_edges[node].iter().enumerate().skip(edge) {
                if index[target.0] == UNVISITED {
                    work.push((node, offset + 1));
                    work.push((target.0, 0));
                    recursed = true;
                    break;
                } else if on_stack[target.0] {
                    low[node] = low[node].min(index[target.0]);
                }
            }
            if recursed {
                continue;
            }
            if low[node] == index[node] {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component.push(NodeId(member));
                    if member == node {
                        break;
                    }
                }
                components.push(component);
            }
            if let Some((parent, _)) = work.last() {
                low[*parent] = low[*parent].min(low[node]);
            }
        }
    }
    components
}

/// Kahn's algorithm over the dependency edges: every node appears after all of
/// its dependencies. Also the cycle check, since a cycle is exactly the case
/// where the queue empties early.
fn topological_order(
    dependencies: &[Vec<NodeId>],
    dependents: &[Vec<NodeId>],
    nodes: &[Node],
) -> Result<Vec<NodeId>, BuildError> {
    let mut remaining: Vec<usize> = dependencies.iter().map(Vec::len).collect();
    let mut queue: std::collections::VecDeque<NodeId> = (0..nodes.len())
        .map(NodeId)
        .filter(|id| remaining[id.0] == 0)
        .collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(id) = queue.pop_front() {
        order.push(id);
        for dependent in &dependents[id.0] {
            remaining[dependent.0] -= 1;
            if remaining[dependent.0] == 0 {
                queue.push_back(*dependent);
            }
        }
    }
    if order.len() == nodes.len() {
        return Ok(order);
    }
    Err(BuildError::Cycle(name_a_cycle(dependencies, &remaining, nodes)))
}

/// One cycle, named, out of the nodes Kahn's algorithm could not drain. Walking
/// dependency edges from any such node must re-enter a cycle, because every
/// stuck node has at least one stuck dependency.
fn name_a_cycle(dependencies: &[Vec<NodeId>], remaining: &[usize], nodes: &[Node]) -> Vec<String> {
    let Some(start) = (0..nodes.len()).map(NodeId).find(|id| remaining[id.0] > 0) else {
        return Vec::new();
    };
    let mut path = Vec::new();
    let mut position = std::collections::HashMap::new();
    let mut current = start;
    loop {
        if let Some(first) = position.insert(current, path.len()) {
            let mut cycle: Vec<String> = path[first..]
                .iter()
                .map(|id: &NodeId| nodes[id.0].key.clone())
                .collect();
            cycle.push(nodes[current.0].key.clone());
            return cycle;
        }
        path.push(current);
        let Some(next) = dependencies[current.0]
            .iter()
            .find(|id| remaining[id.0] > 0)
        else {
            return path.iter().map(|id| nodes[id.0].key.clone()).collect();
        };
        current = *next;
    }
}

/// A validated acyclic graph, with both edge directions materialized.
#[derive(Debug, Clone)]
pub struct Dag {
    pub(crate) nodes: Vec<Node>,
    by_key: HashMap<String, NodeId>,
    /// What each node needs.
    pub(crate) dependencies: Vec<Vec<NodeId>>,
    /// What needs each node. The direction blast radius is measured in.
    pub(crate) dependents: Vec<Vec<NodeId>>,
    /// Dependencies-first topological order.
    pub(crate) order: Vec<NodeId>,
}

impl Dag {
    /// Number of nodes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the graph has no nodes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Number of edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.dependencies.iter().map(Vec::len).sum()
    }

    /// Every node, in insertion order.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter()
    }

    /// The node behind an id.
    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0]
    }

    /// The id for a key.
    #[must_use]
    pub fn id(&self, key: &str) -> Option<NodeId> {
        self.by_key.get(key).copied()
    }

    /// What `id` needs.
    #[must_use]
    pub fn dependencies(&self, id: NodeId) -> &[NodeId] {
        &self.dependencies[id.0]
    }

    /// What needs `id`.
    #[must_use]
    pub fn dependents(&self, id: NodeId) -> &[NodeId] {
        &self.dependents[id.0]
    }
}
