//! Graph data structure: nodes, edges, keys, and the `RelationshipGraph`.
//!
//! Provides the core in-memory graph representation using `slotmap::DenseSlotMap`
//! for O(1) insert/remove/lookup with generational key safety.

use slotmap::{new_key_type, DenseSlotMap};
use std::collections::HashMap;

use crate::model::{DecayPhase, EdgeType, MemoryId, NamespaceId};

// ═══════════════════════════════════════════════════════════════════════
// Key Types
// ═══════════════════════════════════════════════════════════════════════

new_key_type! {
    /// Generational key for graph nodes. 8 bytes, Copy, Eq, Hash.
    /// Catches use-after-delete: if a node is removed and its slot
    /// reused, stale NodeKeys fail validation rather than silently
    /// resolving to the wrong node.
    pub struct NodeKey;
}

new_key_type! {
    /// Generational key for graph edges. Same properties as NodeKey.
    pub struct EdgeKey;
}

// ═══════════════════════════════════════════════════════════════════════
// GraphNode
// ═══════════════════════════════════════════════════════════════════════

/// Lightweight graph-local node. The full MemoryRecord lives in meta.db
/// and the RAM cache -- the graph stores only what traversal and
/// activation need.
#[derive(Clone, Debug)]
pub struct GraphNode {
    /// Primary key -- maps back to the full MemoryRecord in meta.db.
    pub memory_id: MemoryId,

    /// Namespace this memory belongs to. Used for scoped traversal
    /// and namespace-filtered spreading activation.
    pub namespace_id: NamespaceId,

    /// Current decay phase (Full / Summary / Ghost).
    /// Updated by decay sweeps via `update_node_state`.
    pub decay_phase: DecayPhase,

    /// Current retrievability R(t,S), cached from the last decay sweep
    /// or access event. Used by spreading activation to weight neighbor
    /// contributions (a forgotten neighbor provides no boost).
    pub strength: f32,

    /// Index into the namespace's vectors.dat flat buffer. Enables the
    /// auto-linker (CS-11) to retrieve embeddings without a meta.db
    /// round-trip.
    pub vector_slot: u32,

    /// Outgoing edge keys (this node is `source`).
    pub outgoing: Vec<EdgeKey>,

    /// Incoming edge keys (this node is `target`).
    pub incoming: Vec<EdgeKey>,
}

impl GraphNode {
    /// Total degree: outgoing + incoming edges.
    #[inline]
    pub fn degree(&self) -> usize {
        self.outgoing.len() + self.incoming.len()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// GraphEdge
// ═══════════════════════════════════════════════════════════════════════

/// A typed, weighted, directed edge between two graph nodes.
#[derive(Clone, Debug)]
pub struct GraphEdge {
    /// Source node key (the "from" end).
    pub source: NodeKey,

    /// Target node key (the "to" end).
    pub target: NodeKey,

    /// Relationship type. Determines spreading activation weight
    /// and RIF competitor weight (see Spec 05 section 2.4).
    pub edge_type: EdgeType,

    /// Associative strength, 0.0-1.0. For auto-created edges this is
    /// the cosine similarity at creation time. Manual edges default
    /// to 1.0.
    pub weight: f32,

    /// True if this edge was created by the auto-linker (CS-11),
    /// false if manually created via the API.
    pub auto_created: bool,

    /// Milliseconds since Unix epoch when this edge was created.
    pub created_at: i64,
}

// ═══════════════════════════════════════════════════════════════════════
// Traversal Types
// ═══════════════════════════════════════════════════════════════════════

/// Direction filter for BFS traversal.
#[derive(Clone, Debug, Default)]
pub enum TraversalDirection {
    /// Follow both outgoing and incoming edges.
    #[default]
    Both,
    /// Follow only outgoing edges (source -> target).
    Outgoing,
    /// Follow only incoming edges (target -> source).
    Incoming,
}

/// Filter configuration for BFS traversal.
#[derive(Clone, Debug, Default)]
pub struct TraversalFilter {
    /// Edge types to follow (None = all types).
    pub edge_types: Option<Vec<EdgeType>>,
    /// Whether to include ghost memories in results.
    pub include_ghosts: bool,
    /// Whether to traverse through ghost nodes (even if excluded from results).
    pub traverse_through_ghosts: bool,
    /// Direction: outgoing only, incoming only, or both.
    pub direction: TraversalDirection,
    /// Namespace filter -- only traverse within this namespace.
    pub namespace_id: Option<NamespaceId>,
    /// Maximum results to return (None = unlimited).
    /// BFS terminates early once this count is reached.
    pub max_results: Option<usize>,
}

/// A single result from BFS traversal, carrying the edge that led to
/// this node and the hop distance from the start.
#[derive(Clone, Debug)]
pub struct TraversalResult {
    /// The graph key of the discovered node.
    pub node_key: NodeKey,
    /// The MemoryId of the discovered node.
    pub memory_id: MemoryId,
    /// The edge that connected the previous hop to this node.
    pub edge: GraphEdge,
    /// Number of hops from the start node (1 = direct neighbor).
    pub hop_distance: u8,
}

// ═══════════════════════════════════════════════════════════════════════
// GraphError
// ═══════════════════════════════════════════════════════════════════════

/// Errors returned by graph mutation operations.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// A memory was not found in the graph.
    #[error("memory {0:?} not found in graph")]
    MemoryNotFound(MemoryId),

    /// A memory already exists in the graph.
    #[error("memory {0:?} already exists in graph")]
    DuplicateNode(MemoryId),

    /// An edge already exists between two memories.
    #[error("edge already exists between {0:?} and {1:?}")]
    EdgeExists(MemoryId, MemoryId),

    /// An edge key was not found.
    #[error("edge key {0:?} not found")]
    EdgeNotFound(EdgeKey),
}

// ═══════════════════════════════════════════════════════════════════════
// GraphStats
// ═══════════════════════════════════════════════════════════════════════

/// Aggregate graph metrics.
#[derive(Debug, Clone)]
pub struct GraphStats {
    /// Total number of nodes.
    pub node_count: usize,
    /// Total number of edges.
    pub edge_count: usize,
    /// Average degree = (2 * edge_count) / node_count.
    /// Each edge contributes to two nodes' degree.
    pub avg_degree: f32,
    /// Maximum degree of any node.
    pub max_degree: usize,
    /// Number of nodes in the Ghost decay phase.
    pub ghost_node_count: usize,
    /// Edge counts broken down by EdgeType.
    pub edges_by_type: HashMap<EdgeType, usize>,
}

// ═══════════════════════════════════════════════════════════════════════
// RemovalResult
// ═══════════════════════════════════════════════════════════════════════

/// Result of a memory removal with bridging.
#[derive(Debug)]
pub struct RemovalResult {
    /// Edges that were removed from the graph (delete from edges.db).
    pub removed_edges: Vec<GraphEdge>,
    /// Bridge edge that was created, if any (persist to edges.db).
    pub bridge_created: Option<GraphEdge>,
}

// ═══════════════════════════════════════════════════════════════════════
// RelationshipGraph
// ═══════════════════════════════════════════════════════════════════════

/// In-memory relationship graph. All nodes and edges are held in
/// generationally-indexed slotmaps for O(1) insert/remove/lookup.
///
/// This struct is wrapped in `Arc<RwLock<RelationshipGraph>>` (the
/// `SharedGraph` alias in mod.rs) for concurrent access.
pub struct RelationshipGraph {
    /// Node storage. DenseSlotMap keeps elements contiguous for
    /// cache-friendly iteration during decay sweeps and batch
    /// spreading activation.
    ///
    /// `pub(crate)`: accessed directly by graph operations (CS-11)
    /// for traversal, activation, and batch rebuilds.
    pub(crate) nodes: DenseSlotMap<NodeKey, GraphNode>,

    /// Edge storage. Separate from nodes so edges can be iterated
    /// independently (e.g., for persistence serialization).
    ///
    /// `pub(crate)`: accessed directly by graph operations (CS-11)
    /// for traversal and activation edge lookups.
    pub(crate) edges: DenseSlotMap<EdgeKey, GraphEdge>,

    /// MemoryId -> NodeKey lookup. Every node in `nodes` has exactly
    /// one entry here. This is the primary entry point -- callers
    /// address nodes by MemoryId, not NodeKey.
    ///
    /// `pub(crate)`: accessed directly by graph operations (CS-11)
    /// for key resolution in traversal and auto-linking.
    pub(crate) id_index: HashMap<MemoryId, NodeKey>,
}

// ── Constructor ─────────────────────────────────────────────────────

impl RelationshipGraph {
    /// Create a new empty graph with default capacity.
    ///
    /// Pre-allocates for 1,024 nodes and 4,096 edges. These are
    /// starting sizes -- DenseSlotMap grows automatically.
    pub fn new() -> Self {
        Self {
            nodes: DenseSlotMap::with_capacity_and_key(1_024),
            edges: DenseSlotMap::with_capacity_and_key(4_096),
            id_index: HashMap::with_capacity(1_024),
        }
    }

    /// Create a graph pre-sized for the given node count.
    /// Used at startup when the node count is known from meta.db.
    pub fn with_capacity(node_capacity: usize) -> Self {
        let edge_capacity = node_capacity * 5; // avg degree ~10 -> 5 edges per node
        Self {
            nodes: DenseSlotMap::with_capacity_and_key(node_capacity),
            edges: DenseSlotMap::with_capacity_and_key(edge_capacity),
            id_index: HashMap::with_capacity(node_capacity),
        }
    }
}

impl Default for RelationshipGraph {
    fn default() -> Self {
        Self::new()
    }
}

// ── Node Operations ─────────────────────────────────────────────────

impl RelationshipGraph {
    /// Insert a node for a newly persisted memory.
    ///
    /// Returns the assigned NodeKey. Fails with `DuplicateNode` if a
    /// node for this MemoryId already exists.
    pub fn add_node(
        &mut self,
        memory_id: MemoryId,
        namespace_id: NamespaceId,
        decay_phase: DecayPhase,
        strength: f32,
        vector_slot: u32,
    ) -> Result<NodeKey, GraphError> {
        if self.id_index.contains_key(&memory_id) {
            return Err(GraphError::DuplicateNode(memory_id));
        }

        let node = GraphNode {
            memory_id,
            namespace_id,
            decay_phase,
            strength,
            vector_slot,
            outgoing: Vec::new(),
            incoming: Vec::new(),
        };

        let key = self.nodes.insert(node);
        self.id_index.insert(memory_id, key);
        Ok(key)
    }

    /// Remove a node and ALL its edges. Returns the removed edges
    /// so the caller can delete them from edges.db.
    ///
    /// This is the simple removal path (no bridging). For ghost
    /// deletion with pass-through bridging, use
    /// `remove_memory_with_bridging`.
    pub fn remove_node(
        &mut self,
        memory_id: MemoryId,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        let node_key = self.resolve_key(memory_id)?;

        let node = self
            .nodes
            .get(node_key)
            .ok_or(GraphError::MemoryNotFound(memory_id))?;

        // Collect all edge keys before mutating
        let edge_keys: Vec<EdgeKey> = node
            .outgoing
            .iter()
            .chain(node.incoming.iter())
            .copied()
            .collect();

        let mut removed_edges = Vec::with_capacity(edge_keys.len());

        for ek in edge_keys {
            if let Some(edge) = self.edges.remove(ek) {
                // Remove this edge key from the OTHER node's lists
                let other_key = if edge.source == node_key {
                    edge.target
                } else {
                    edge.source
                };
                if let Some(other_node) = self.nodes.get_mut(other_key) {
                    other_node.outgoing.retain(|k| *k != ek);
                    other_node.incoming.retain(|k| *k != ek);
                }
                removed_edges.push(edge);
            }
        }

        self.nodes.remove(node_key);
        self.id_index.remove(&memory_id);

        Ok(removed_edges)
    }

    /// Update a node's cached decay state. Called by decay sweeps
    /// after recomputing R(t,S) and checking phase transitions.
    pub fn update_node_state(
        &mut self,
        memory_id: MemoryId,
        decay_phase: DecayPhase,
        strength: f32,
    ) -> Result<(), GraphError> {
        let key = self.resolve_key(memory_id)?;
        let node = self
            .nodes
            .get_mut(key)
            .ok_or(GraphError::MemoryNotFound(memory_id))?;
        node.decay_phase = decay_phase;
        node.strength = strength;
        Ok(())
    }

    /// Check if a memory has a node in the graph.
    #[inline]
    pub fn contains(&self, memory_id: &MemoryId) -> bool {
        self.id_index.contains_key(memory_id)
    }

    /// Get a node's total degree (outgoing + incoming).
    /// Returns 0 if the memory is not in the graph.
    pub fn degree(&self, memory_id: &MemoryId) -> usize {
        self.id_index
            .get(memory_id)
            .and_then(|k| self.nodes.get(*k))
            .map(|n| n.degree())
            .unwrap_or(0)
    }

    /// Total number of nodes in the graph.
    #[inline]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Read-only access to a node by MemoryId.
    pub fn get_node(&self, memory_id: &MemoryId) -> Option<&GraphNode> {
        self.id_index
            .get(memory_id)
            .and_then(|k| self.nodes.get(*k))
    }

    /// Read-only access to a node by NodeKey.
    pub fn get_node_by_key(&self, key: NodeKey) -> Option<&GraphNode> {
        self.nodes.get(key)
    }
}

// ── Edge Operations ─────────────────────────────────────────────────

impl RelationshipGraph {
    /// Add a directed edge between two memories.
    ///
    /// Returns the assigned EdgeKey. Fails if:
    /// - Either memory is not in the graph (`MemoryNotFound`)
    /// - An edge of the SAME type already exists between them (`EdgeExists`)
    ///
    /// Different edge types between the same pair are allowed (e.g.,
    /// Associative + Entity) because they carry different semantic
    /// meaning and different activation/RIF weights.
    pub fn add_edge(
        &mut self,
        source: MemoryId,
        target: MemoryId,
        edge_type: EdgeType,
        weight: f32,
        auto_created: bool,
    ) -> Result<EdgeKey, GraphError> {
        let source_key = self.resolve_key(source)?;
        let target_key = self.resolve_key(target)?;

        // Check for existing edge of the SAME type (either direction).
        // Different edge types between the same pair are allowed (e.g.,
        // Associative + Entity) because they carry different semantic
        // meaning and different activation/RIF weights.
        if self.has_typed_edge_between(source_key, target_key, edge_type) {
            return Err(GraphError::EdgeExists(source, target));
        }

        let edge = GraphEdge {
            source: source_key,
            target: target_key,
            edge_type,
            weight: weight.clamp(0.0, 1.0),
            auto_created,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
        };

        let edge_key = self.edges.insert(edge);

        // Update adjacency lists on both nodes
        self.nodes
            .get_mut(source_key)
            .expect("source validated above")
            .outgoing
            .push(edge_key);
        self.nodes
            .get_mut(target_key)
            .expect("target validated above")
            .incoming
            .push(edge_key);

        Ok(edge_key)
    }

    /// Remove an edge by its key. Returns the removed edge.
    ///
    /// Also removes the edge key from the source's `outgoing` and
    /// the target's `incoming` lists.
    pub fn remove_edge(&mut self, edge_key: EdgeKey) -> Result<GraphEdge, GraphError> {
        let edge = self
            .edges
            .remove(edge_key)
            .ok_or(GraphError::EdgeNotFound(edge_key))?;

        if let Some(src) = self.nodes.get_mut(edge.source) {
            src.outgoing.retain(|k| *k != edge_key);
        }
        if let Some(tgt) = self.nodes.get_mut(edge.target) {
            tgt.incoming.retain(|k| *k != edge_key);
        }

        Ok(edge)
    }

    /// Remove the edge between two memories with a specific type.
    /// Returns true if an edge was found and removed.
    pub fn remove_edge_between(
        &mut self,
        source: MemoryId,
        target: MemoryId,
        edge_type: EdgeType,
    ) -> bool {
        let (source_key, target_key) = match (
            self.id_index.get(&source).copied(),
            self.id_index.get(&target).copied(),
        ) {
            (Some(s), Some(t)) => (s, t),
            _ => return false,
        };

        // Find the edge key in source's outgoing list
        let edge_key = self.nodes.get(source_key).and_then(|node| {
            node.outgoing
                .iter()
                .find(|&&ek| {
                    self.edges
                        .get(ek)
                        .is_some_and(|e| e.target == target_key && e.edge_type == edge_type)
                })
                .copied()
        });

        match edge_key {
            Some(ek) => self.remove_edge(ek).is_ok(),
            None => false,
        }
    }

    /// List all edges connected to a memory (both outgoing and incoming).
    pub fn edges_for(&self, memory_id: &MemoryId) -> Vec<&GraphEdge> {
        let Some(&key) = self.id_index.get(memory_id) else {
            return Vec::new();
        };
        let Some(node) = self.nodes.get(key) else {
            return Vec::new();
        };

        node.outgoing
            .iter()
            .chain(node.incoming.iter())
            .filter_map(|ek| self.edges.get(*ek))
            .collect()
    }

    /// Outgoing edges only (this memory is source).
    pub fn outgoing_edges(&self, memory_id: &MemoryId) -> Vec<&GraphEdge> {
        let Some(&key) = self.id_index.get(memory_id) else {
            return Vec::new();
        };
        let Some(node) = self.nodes.get(key) else {
            return Vec::new();
        };
        node.outgoing
            .iter()
            .filter_map(|ek| self.edges.get(*ek))
            .collect()
    }

    /// Incoming edges only (this memory is target).
    pub fn incoming_edges(&self, memory_id: &MemoryId) -> Vec<&GraphEdge> {
        let Some(&key) = self.id_index.get(memory_id) else {
            return Vec::new();
        };
        let Some(node) = self.nodes.get(key) else {
            return Vec::new();
        };
        node.incoming
            .iter()
            .filter_map(|ek| self.edges.get(*ek))
            .collect()
    }

    /// Total number of edges in the graph.
    #[inline]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Read-only access to an edge by key.
    pub fn get_edge(&self, key: EdgeKey) -> Option<&GraphEdge> {
        self.edges.get(key)
    }
}

// ── Statistics ───────────────────────────────────────────────────────

impl RelationshipGraph {
    /// Compute graph-wide statistics for monitoring and debugging.
    pub fn stats(&self) -> GraphStats {
        let node_count = self.nodes.len();
        let edge_count = self.edges.len();

        let mut max_degree: usize = 0;
        let mut ghost_count: usize = 0;
        let mut edges_by_type: HashMap<EdgeType, usize> = HashMap::new();

        for (_, node) in &self.nodes {
            let d = node.degree();
            if d > max_degree {
                max_degree = d;
            }
            if node.decay_phase == DecayPhase::Ghost {
                ghost_count += 1;
            }
        }

        for (_, edge) in &self.edges {
            *edges_by_type.entry(edge.edge_type).or_insert(0) += 1;
        }

        let avg_degree = if node_count > 0 {
            (edge_count as f32 * 2.0) / node_count as f32
        } else {
            0.0
        };

        GraphStats {
            node_count,
            edge_count,
            avg_degree,
            max_degree,
            ghost_node_count: ghost_count,
            edges_by_type,
        }
    }
}

// ── Ghost Bridging ──────────────────────────────────────────────────

impl RelationshipGraph {
    /// Remove a ghost memory and handle its edges with pass-through bridging.
    ///
    /// Returns a `RemovalResult` containing removed edges (caller must
    /// delete from edges.db) and any bridge edge created (caller must
    /// persist to edges.db).
    ///
    /// # Bridge creation
    /// Only creates a bridge for pass-through nodes (1 in, 1 out, same
    /// edge type). Bridge weight = w_in * w_out * 0.9, minimum 0.1.
    pub fn remove_memory_with_bridging(&mut self, memory_id: MemoryId) -> RemovalResult {
        let Some(&node_key) = self.id_index.get(&memory_id) else {
            return RemovalResult {
                removed_edges: Vec::new(),
                bridge_created: None,
            };
        };
        let Some(node) = self.nodes.get(node_key) else {
            return RemovalResult {
                removed_edges: Vec::new(),
                bridge_created: None,
            };
        };

        let in_degree = node.incoming.len();
        let out_degree = node.outgoing.len();
        let mut bridge_created: Option<GraphEdge> = None;

        // Tier 2: Pass-through node (1 in, 1 out, same edge type) -- bridge
        let should_bridge = in_degree == 1
            && out_degree == 1
            && {
                let in_edge = self.edges.get(node.incoming[0]);
                let out_edge = self.edges.get(node.outgoing[0]);
                match (in_edge, out_edge) {
                    (Some(ie), Some(oe)) => ie.edge_type == oe.edge_type,
                    _ => false,
                }
            };

        if should_bridge {
            if let (Some(in_edge), Some(out_edge)) = (
                self.edges.get(node.incoming[0]).cloned(),
                self.edges.get(node.outgoing[0]).cloned(),
            ) {
                let bridge_weight = in_edge.weight * out_edge.weight * 0.9;
                let predecessor = in_edge.source;
                let successor = out_edge.target;

                // Only bridge if no edge already exists and weight is sufficient
                let edge_exists = self.edge_exists_between_keys(predecessor, successor);

                if !edge_exists && bridge_weight > 0.1 {
                    let bridge = GraphEdge {
                        source: predecessor,
                        target: successor,
                        edge_type: in_edge.edge_type,
                        weight: bridge_weight,
                        auto_created: true,
                        created_at: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64,
                    };

                    let ek = self.edges.insert(bridge.clone());
                    if let Some(pred_node) = self.nodes.get_mut(predecessor) {
                        pred_node.outgoing.push(ek);
                    }
                    if let Some(succ_node) = self.nodes.get_mut(successor) {
                        succ_node.incoming.push(ek);
                    }
                    bridge_created = Some(bridge);
                }
            }
        }

        // Remove all edges connected to this node
        // Re-fetch the node since we may have mutated other nodes above
        let node = self.nodes.get(node_key).expect("node still exists");
        let all_edge_keys: Vec<EdgeKey> = node
            .outgoing
            .iter()
            .chain(node.incoming.iter())
            .copied()
            .collect();

        let mut removed_edges = Vec::with_capacity(all_edge_keys.len());

        for ek in all_edge_keys {
            if let Some(edge) = self.edges.remove(ek) {
                // Remove from the other node's edge lists
                let other_key = if edge.source == node_key {
                    edge.target
                } else {
                    edge.source
                };
                if let Some(other_node) = self.nodes.get_mut(other_key) {
                    other_node.outgoing.retain(|e| *e != ek);
                    other_node.incoming.retain(|e| *e != ek);
                }
                removed_edges.push(edge);
            }
        }

        // Remove the node itself
        self.nodes.remove(node_key);
        self.id_index.remove(&memory_id);

        RemovalResult {
            removed_edges,
            bridge_created,
        }
    }

    /// Check if any edge exists from source to target (forward direction only).
    fn edge_exists_between_keys(&self, source: NodeKey, target: NodeKey) -> bool {
        if let Some(source_node) = self.nodes.get(source) {
            for &ek in &source_node.outgoing {
                if let Some(edge) = self.edges.get(ek) {
                    if edge.target == target {
                        return true;
                    }
                }
            }
        }
        false
    }
}

// ── Internal Helpers ────────────────────────────────────────────────

impl RelationshipGraph {
    /// Resolve a MemoryId to its NodeKey, or error.
    fn resolve_key(&self, memory_id: MemoryId) -> Result<NodeKey, GraphError> {
        self.id_index
            .get(&memory_id)
            .copied()
            .ok_or(GraphError::MemoryNotFound(memory_id))
    }


    /// Check if an edge of a specific type exists between two node keys (either direction).
    fn has_typed_edge_between(&self, a: NodeKey, b: NodeKey, edge_type: EdgeType) -> bool {
        if let Some(node_a) = self.nodes.get(a) {
            // Check a's outgoing for edges targeting b with matching type
            for &ek in &node_a.outgoing {
                if let Some(edge) = self.edges.get(ek) {
                    if edge.target == b && edge.edge_type == edge_type {
                        return true;
                    }
                }
            }
            // Also check a's incoming (covers b->a direction)
            for &ek in &node_a.incoming {
                if let Some(edge) = self.edges.get(ek) {
                    if edge.source == b && edge.edge_type == edge_type {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Provide read-only access to the node slotmap for iteration.
    /// Used by CS-11 (spreading activation batch sweep) and CS-04
    /// (decay sweep runner).
    pub fn iter_nodes(&self) -> impl Iterator<Item = (NodeKey, &GraphNode)> {
        self.nodes.iter()
    }

    /// Provide read-only access to the edge slotmap for iteration.
    /// Used by persistence serialization (all_edges).
    pub fn iter_edges(&self) -> impl Iterator<Item = (EdgeKey, &GraphEdge)> {
        self.edges.iter()
    }

    /// Look up a NodeKey by MemoryId. Public for CS-11 operations
    /// that need to work with keys directly.
    pub fn resolve(&self, memory_id: &MemoryId) -> Option<NodeKey> {
        self.id_index.get(memory_id).copied()
    }
}
