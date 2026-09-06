use alloc::vec::Vec;

use crate::{EpollGraphId, EpollId};

/// Immutable finite limits for one externally serialized epoll graph domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpollGraphLimits {
    node_capacity: usize,
    edge_capacity: usize,
    max_parents_per_node: usize,
    max_nesting: usize,
    max_walk_steps: usize,
}

impl EpollGraphLimits {
    /// Creates finite graph, reverse-parent, nesting, and walk limits.
    ///
    /// Zero is a valid honest capacity. `usize::MAX` is rejected for every
    /// field rather than being exposed as an accidental unlimited policy.
    pub const fn try_new(
        node_capacity: usize,
        edge_capacity: usize,
        max_parents_per_node: usize,
        max_nesting: usize,
        max_walk_steps: usize,
    ) -> Result<Self, GraphError> {
        if node_capacity == usize::MAX
            || edge_capacity == usize::MAX
            || max_parents_per_node == usize::MAX
            || max_nesting == usize::MAX
            || max_walk_steps == usize::MAX
        {
            return Err(GraphError::Unbounded);
        }
        Ok(Self {
            node_capacity,
            edge_capacity,
            max_parents_per_node,
            max_nesting,
            max_walk_steps,
        })
    }

    /// Maximum simultaneously registered epoll instances.
    pub const fn node_capacity(self) -> usize {
        self.node_capacity
    }

    /// Maximum simultaneously published interests in the graph domain.
    pub const fn edge_capacity(self) -> usize {
        self.edge_capacity
    }

    /// Maximum distinct epoll parents of one child epoll.
    pub const fn max_parents_per_node(self) -> usize {
        self.max_parents_per_node
    }

    /// Maximum epoll-to-leaf path length.
    pub const fn max_nesting(self) -> usize {
        self.max_nesting
    }

    /// Maximum graph nodes/edges examined by one publication attempt.
    pub const fn max_walk_steps(self) -> usize {
        self.max_walk_steps
    }
}

/// Generation-tagged registration of one epoll instance in a graph domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GraphNodeToken {
    graph: EpollGraphId,
    slot: usize,
    generation: u64,
    epoll: EpollId,
}

impl GraphNodeToken {
    /// Returns the graph domain that issued the token.
    pub const fn graph(self) -> EpollGraphId {
        self.graph
    }

    /// Returns the registered epoll instance identity.
    pub const fn epoll(self) -> EpollId {
        self.epoll
    }

    /// Returns the opaque registration generation.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Generation-tagged ownership of one epoll-to-object interest edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GraphEdgeToken {
    graph: EpollGraphId,
    slot: usize,
    generation: u64,
}

impl GraphEdgeToken {
    /// Returns the graph domain that issued the token.
    pub const fn graph(self) -> EpollGraphId {
        self.graph
    }

    /// Returns the opaque edge generation.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Epoll graph operation failure before Linux errno mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphError {
    /// A configured field used `usize::MAX` as an accidental unlimited value.
    Unbounded,
    /// Fallible fixed storage reservation failed.
    NoMemory,
    /// Node or edge storage has reached its admitted capacity.
    Capacity,
    /// The epoll identity is already registered in this graph.
    DuplicateNode,
    /// The exact node still owns outgoing or incoming interests.
    Busy,
    /// A node or edge token is foreign, removed, or from an older generation.
    StaleToken,
    /// An epoll attempted to directly watch itself.
    SelfCycle,
    /// The proposed edge would introduce a directed cycle.
    Cycle,
    /// The proposed edge would exceed the configured path nesting limit.
    Nesting,
    /// The child already has the maximum number of distinct epoll parents.
    ParentLimit,
    /// Validation exhausted its explicit graph-walk work budget.
    WalkLimit,
    /// No future unique registration or edge generation can be issued.
    GenerationExhausted,
}

#[derive(Debug, Clone, Copy)]
struct Node {
    epoll: EpollId,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct Edge {
    generation: u64,
    parent: GraphNodeToken,
    child: Option<GraphNodeToken>,
}

/// Fixed-capacity epoll topology and reverse-parent accounting.
///
/// The consumer serializes every graph mutation with one domain lock. An
/// adapter normally publishes the graph edge and its [`crate::EpollCore`]
/// interest in one such critical section, rolling the first operation back if
/// the second fails. Construction performs every allocation; graph mutation
/// and validation do not allocate, invoke callbacks, or destroy consumers.
pub struct EpollGraph {
    id: EpollGraphId,
    limits: EpollGraphLimits,
    nodes: Vec<Option<Node>>,
    edges: Vec<Option<Edge>>,
    next_generation: u64,
}

impl EpollGraph {
    /// Fallibly reserves all graph storage up front.
    pub fn try_new(id: EpollGraphId, limits: EpollGraphLimits) -> Result<Self, GraphError> {
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(limits.node_capacity)
            .map_err(|_| GraphError::NoMemory)?;
        nodes.resize(limits.node_capacity, None);

        let mut edges = Vec::new();
        edges
            .try_reserve_exact(limits.edge_capacity)
            .map_err(|_| GraphError::NoMemory)?;
        edges.resize(limits.edge_capacity, None);

        Ok(Self {
            id,
            limits,
            nodes,
            edges,
            next_generation: 1,
        })
    }

    /// Returns the graph domain identity.
    pub const fn id(&self) -> EpollGraphId {
        self.id
    }

    /// Returns the immutable configured limits.
    pub const fn limits(&self) -> EpollGraphLimits {
        self.limits
    }

    /// Returns the number of registered epoll nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.iter().flatten().count()
    }

    /// Returns the number of published interest edges, including leaf edges.
    pub fn edge_count(&self) -> usize {
        self.edges.iter().flatten().count()
    }

    fn allocate_generation(&mut self) -> Result<u64, GraphError> {
        let generation = self.next_generation;
        self.next_generation = generation
            .checked_add(1)
            .ok_or(GraphError::GenerationExhausted)?;
        Ok(generation)
    }

    fn validate_node(&self, token: GraphNodeToken) -> Result<usize, GraphError> {
        if token.graph != self.id {
            return Err(GraphError::StaleToken);
        }
        match self.nodes.get(token.slot).and_then(Option::as_ref) {
            Some(node) if node.generation == token.generation && node.epoll == token.epoll => {
                Ok(token.slot)
            }
            _ => Err(GraphError::StaleToken),
        }
    }

    /// Registers one epoll instance without publishing any interest edges.
    pub fn register(&mut self, epoll: EpollId) -> Result<GraphNodeToken, GraphError> {
        if self.nodes.iter().flatten().any(|node| node.epoll == epoll) {
            return Err(GraphError::DuplicateNode);
        }
        let slot = self
            .nodes
            .iter()
            .position(Option::is_none)
            .ok_or(GraphError::Capacity)?;
        let generation = self.allocate_generation()?;
        self.nodes[slot] = Some(Node { epoll, generation });
        Ok(GraphNodeToken {
            graph: self.id,
            slot,
            generation,
            epoll,
        })
    }

    /// Removes an exact epoll registration after all its edges are detached.
    pub fn unregister(&mut self, node: GraphNodeToken) -> Result<(), GraphError> {
        self.validate_node(node)?;
        if self
            .edges
            .iter()
            .flatten()
            .any(|edge| edge.parent == node || edge.child.is_some_and(|child| child == node))
        {
            return Err(GraphError::Busy);
        }
        self.nodes[node.slot] = None;
        Ok(())
    }

    fn spend(budget: &mut usize) -> Result<(), GraphError> {
        if *budget == 0 {
            return Err(GraphError::WalkLimit);
        }
        *budget -= 1;
        Ok(())
    }

    fn reaches(
        &self,
        current: GraphNodeToken,
        target: GraphNodeToken,
        budget: &mut usize,
        path_len: usize,
    ) -> Result<bool, GraphError> {
        Self::spend(budget)?;
        if current == target {
            return Ok(true);
        }
        if path_len > self.nodes.len() {
            return Err(GraphError::Cycle);
        }
        for edge in self.edges.iter().flatten() {
            Self::spend(budget)?;
            if edge.parent == current {
                if let Some(child) = edge.child {
                    if self.reaches(child, target, budget, path_len.saturating_add(1))? {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    fn max_descendant_depth(
        &self,
        current: GraphNodeToken,
        budget: &mut usize,
        path_len: usize,
    ) -> Result<usize, GraphError> {
        Self::spend(budget)?;
        if path_len > self.nodes.len() {
            return Err(GraphError::Cycle);
        }
        let mut maximum = 0usize;
        for edge in self.edges.iter().flatten() {
            Self::spend(budget)?;
            if edge.parent != current {
                continue;
            }
            let below = match edge.child {
                Some(child) => {
                    self.max_descendant_depth(child, budget, path_len.saturating_add(1))?
                }
                None => 0,
            };
            maximum = maximum.max(below.checked_add(1).ok_or(GraphError::Nesting)?);
        }
        Ok(maximum)
    }

    fn max_parent_depth(
        &self,
        current: GraphNodeToken,
        budget: &mut usize,
        path_len: usize,
    ) -> Result<usize, GraphError> {
        Self::spend(budget)?;
        if path_len > self.nodes.len() {
            return Err(GraphError::Cycle);
        }
        let mut maximum = 0usize;
        for edge in self.edges.iter().flatten() {
            Self::spend(budget)?;
            if edge.child != Some(current) {
                continue;
            }
            let above = self.max_parent_depth(edge.parent, budget, path_len.saturating_add(1))?;
            maximum = maximum.max(above.checked_add(1).ok_or(GraphError::Nesting)?);
        }
        Ok(maximum)
    }

    fn distinct_parent_count(&self, child: GraphNodeToken) -> usize {
        let mut count = 0usize;
        for (index, edge) in self.edges.iter().enumerate() {
            let Some(edge) = edge else {
                continue;
            };
            if edge.child != Some(child) {
                continue;
            }
            let appeared_earlier = self.edges[..index]
                .iter()
                .flatten()
                .any(|earlier| earlier.child == Some(child) && earlier.parent == edge.parent);
            if !appeared_earlier {
                count = count.saturating_add(1);
            }
        }
        count
    }

    /// Publishes one interest from `parent` to an epoll `child`, or to a
    /// non-epoll leaf when `child` is `None`.
    ///
    /// Duplicate parent/child edges are retained independently because Linux
    /// interest identity also includes the descriptor used for `ADD`.
    pub fn add_interest(
        &mut self,
        parent: GraphNodeToken,
        child: Option<GraphNodeToken>,
    ) -> Result<GraphEdgeToken, GraphError> {
        self.validate_node(parent)?;
        if let Some(child) = child {
            self.validate_node(child)?;
            if parent == child {
                return Err(GraphError::SelfCycle);
            }
            let already_parent = self
                .edges
                .iter()
                .flatten()
                .any(|edge| edge.child == Some(child) && edge.parent == parent);
            if !already_parent
                && self.distinct_parent_count(child) >= self.limits.max_parents_per_node
            {
                return Err(GraphError::ParentLimit);
            }
        }

        let slot = self
            .edges
            .iter()
            .position(Option::is_none)
            .ok_or(GraphError::Capacity)?;

        let mut budget = self.limits.max_walk_steps;
        if let Some(child) = child {
            if self.reaches(child, parent, &mut budget, 0)? {
                return Err(GraphError::Cycle);
            }
        }
        let parent_depth = self.max_parent_depth(parent, &mut budget, 0)?;
        let child_depth = match child {
            Some(child) => self.max_descendant_depth(child, &mut budget, 0)?,
            None => 0,
        };
        let combined = parent_depth
            .checked_add(1)
            .and_then(|depth| depth.checked_add(child_depth))
            .ok_or(GraphError::Nesting)?;
        if combined > self.limits.max_nesting {
            return Err(GraphError::Nesting);
        }

        let generation = self.allocate_generation()?;
        self.edges[slot] = Some(Edge {
            generation,
            parent,
            child,
        });
        Ok(GraphEdgeToken {
            graph: self.id,
            slot,
            generation,
        })
    }

    /// Removes one exact interest edge, rejecting foreign or reused tokens.
    pub fn remove_interest(&mut self, token: GraphEdgeToken) -> Result<(), GraphError> {
        if token.graph != self.id {
            return Err(GraphError::StaleToken);
        }
        let Some(edge) = self.edges.get(token.slot).and_then(Option::as_ref) else {
            return Err(GraphError::StaleToken);
        };
        if edge.generation != token.generation {
            return Err(GraphError::StaleToken);
        }
        self.edges[token.slot] = None;
        Ok(())
    }

    /// Returns the number of distinct epoll parents currently watching `node`.
    pub fn parent_count(&self, node: GraphNodeToken) -> Result<usize, GraphError> {
        self.validate_node(node)?;
        Ok(self.distinct_parent_count(node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_id(raw: u64) -> EpollGraphId {
        EpollGraphId::new(raw).unwrap()
    }

    fn epoll_id(raw: u64) -> EpollId {
        EpollId::new(raw).unwrap()
    }

    fn limits(nodes: usize, edges: usize, parents: usize, nesting: usize) -> EpollGraphLimits {
        EpollGraphLimits::try_new(nodes, edges, parents, nesting, 512).unwrap()
    }

    #[test]
    fn zero_capacity_and_unbounded_configuration_are_honest() {
        assert_eq!(
            EpollGraphLimits::try_new(usize::MAX, 0, 0, 0, 0),
            Err(GraphError::Unbounded)
        );
        let mut graph = EpollGraph::try_new(graph_id(1), limits(0, 0, 0, 0)).unwrap();
        assert_eq!(graph.register(epoll_id(1)), Err(GraphError::Capacity));
    }

    #[test]
    fn cycle_and_nesting_are_rejected_before_publication() {
        let mut graph = EpollGraph::try_new(graph_id(1), limits(5, 8, 4, 3)).unwrap();
        let a = graph.register(epoll_id(1)).unwrap();
        let b = graph.register(epoll_id(2)).unwrap();
        let c = graph.register(epoll_id(3)).unwrap();
        let d = graph.register(epoll_id(4)).unwrap();

        graph.add_interest(a, Some(b)).unwrap();
        graph.add_interest(b, Some(c)).unwrap();
        assert_eq!(graph.add_interest(c, Some(a)), Err(GraphError::Cycle));
        assert_eq!(graph.edge_count(), 2);

        graph.add_interest(c, Some(d)).unwrap();
        assert_eq!(graph.add_interest(d, None), Err(GraphError::Nesting));
        assert_eq!(graph.edge_count(), 3);
    }

    #[test]
    fn adding_a_parent_revalidates_existing_child_depth() {
        let mut graph = EpollGraph::try_new(graph_id(1), limits(5, 8, 4, 3)).unwrap();
        let root = graph.register(epoll_id(1)).unwrap();
        let middle = graph.register(epoll_id(2)).unwrap();
        let leaf_owner = graph.register(epoll_id(3)).unwrap();

        graph.add_interest(middle, Some(leaf_owner)).unwrap();
        graph.add_interest(leaf_owner, None).unwrap();
        graph.add_interest(root, Some(middle)).unwrap();

        let outer = graph.register(epoll_id(4)).unwrap();
        assert_eq!(
            graph.add_interest(outer, Some(root)),
            Err(GraphError::Nesting)
        );
    }

    #[test]
    fn reverse_parent_limit_counts_unique_parents_not_duplicate_edges() {
        let mut graph = EpollGraph::try_new(graph_id(1), limits(4, 8, 1, 4)).unwrap();
        let first = graph.register(epoll_id(1)).unwrap();
        let second = graph.register(epoll_id(2)).unwrap();
        let child = graph.register(epoll_id(3)).unwrap();

        let first_edge = graph.add_interest(first, Some(child)).unwrap();
        let duplicate = graph.add_interest(first, Some(child)).unwrap();
        assert_eq!(graph.parent_count(child), Ok(1));
        assert_eq!(
            graph.add_interest(second, Some(child)),
            Err(GraphError::ParentLimit)
        );

        graph.remove_interest(first_edge).unwrap();
        assert_eq!(graph.parent_count(child), Ok(1));
        graph.remove_interest(duplicate).unwrap();
        assert_eq!(graph.parent_count(child), Ok(0));
        graph.add_interest(second, Some(child)).unwrap();
    }

    #[test]
    fn unregister_requires_all_forward_and_reverse_edges_to_be_removed() {
        let mut graph = EpollGraph::try_new(graph_id(1), limits(3, 3, 2, 3)).unwrap();
        let parent = graph.register(epoll_id(1)).unwrap();
        let child = graph.register(epoll_id(2)).unwrap();
        let edge = graph.add_interest(parent, Some(child)).unwrap();

        assert_eq!(graph.unregister(parent), Err(GraphError::Busy));
        assert_eq!(graph.unregister(child), Err(GraphError::Busy));
        graph.remove_interest(edge).unwrap();
        graph.unregister(parent).unwrap();
        graph.unregister(child).unwrap();
    }

    #[test]
    fn stale_and_foreign_edge_tokens_cannot_remove_reused_edges() {
        let mut first = EpollGraph::try_new(graph_id(1), limits(2, 1, 1, 2)).unwrap();
        let parent = first.register(epoll_id(1)).unwrap();
        let edge = first.add_interest(parent, None).unwrap();
        first.remove_interest(edge).unwrap();
        let replacement = first.add_interest(parent, None).unwrap();
        assert_eq!(first.remove_interest(edge), Err(GraphError::StaleToken));

        let mut second = EpollGraph::try_new(graph_id(2), limits(1, 1, 1, 1)).unwrap();
        let other = second.register(epoll_id(2)).unwrap();
        let foreign = second.add_interest(other, None).unwrap();
        assert_eq!(first.remove_interest(foreign), Err(GraphError::StaleToken));
        first.remove_interest(replacement).unwrap();
    }

    #[test]
    fn graph_walk_budget_fails_closed_without_publication() {
        let limits = EpollGraphLimits::try_new(3, 3, 2, 3, 0).unwrap();
        let mut graph = EpollGraph::try_new(graph_id(1), limits).unwrap();
        let parent = graph.register(epoll_id(1)).unwrap();
        assert_eq!(graph.add_interest(parent, None), Err(GraphError::WalkLimit));
        assert_eq!(graph.edge_count(), 0);
    }
}
