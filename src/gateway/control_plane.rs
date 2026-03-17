//! Control plane for multi-node orchestration.
//!
//! Wraps the existing `NodeRegistry` to add capability tracking,
//! health status, and control events without replacing the underlying registry.

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Capabilities that a node can advertise.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NodeCapability {
    /// Can run LLM provider inference
    Inference,
    /// Can execute tools (shell, file, etc.)
    ToolExecution,
    /// Has hardware peripherals attached
    Hardware,
    /// Can serve as a gateway
    Gateway,
    /// Has memory/storage backend
    Storage,
    /// Custom capability
    Custom(String),
}

/// Health status of a node.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Node is healthy and responding
    Healthy,
    /// Node has missed recent heartbeats
    Degraded,
    /// Node is not responding
    Unreachable,
    /// Node is intentionally offline
    Offline,
}

/// Extended information about a registered node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Unique node identifier
    pub id: String,
    /// Human-readable node name
    pub name: Option<String>,
    /// Node's advertised address (host:port)
    pub address: Option<String>,
    /// Node version
    pub version: Option<String>,
    /// Capabilities the node supports
    pub capabilities: Vec<NodeCapability>,
    /// Current health status
    pub status: NodeStatus,
    /// When this node was first registered
    pub registered_at: DateTime<Utc>,
    /// Last successful heartbeat
    pub last_heartbeat: DateTime<Utc>,
    /// Consecutive missed heartbeats
    pub missed_heartbeats: u32,
    /// Arbitrary metadata
    pub metadata: HashMap<String, String>,
}

/// Events emitted by the control plane.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    /// A node registered
    NodeRegistered {
        node_id: String,
        name: Option<String>,
        capabilities: Vec<NodeCapability>,
        timestamp: DateTime<Utc>,
    },
    /// A node was deregistered
    NodeDeregistered {
        node_id: String,
        timestamp: DateTime<Utc>,
    },
    /// A node's status changed
    NodeStatusChanged {
        node_id: String,
        old_status: NodeStatus,
        new_status: NodeStatus,
        timestamp: DateTime<Utc>,
    },
    /// A node sent a heartbeat
    NodeHeartbeat {
        node_id: String,
        timestamp: DateTime<Utc>,
    },
}

/// The control plane manages node lifecycle and health.
pub struct ControlPlane {
    nodes: Mutex<HashMap<String, NodeInfo>>,
    max_nodes: usize,
    event_tx: tokio::sync::broadcast::Sender<serde_json::Value>,
    /// How many missed heartbeats before marking degraded
    degraded_threshold: u32,
    /// How many missed heartbeats before marking unreachable
    unreachable_threshold: u32,
}

impl ControlPlane {
    pub fn new(
        max_nodes: usize,
        event_tx: tokio::sync::broadcast::Sender<serde_json::Value>,
    ) -> Self {
        Self {
            nodes: Mutex::new(HashMap::new()),
            max_nodes,
            event_tx,
            degraded_threshold: 3,
            unreachable_threshold: 10,
        }
    }

    /// Register a new node. Returns false if capacity is reached.
    pub fn register(&self, info: NodeInfo) -> bool {
        let mut nodes = self.nodes.lock();
        if nodes.len() >= self.max_nodes && !nodes.contains_key(&info.id) {
            return false;
        }
        let event = ControlEvent::NodeRegistered {
            node_id: info.id.clone(),
            name: info.name.clone(),
            capabilities: info.capabilities.clone(),
            timestamp: Utc::now(),
        };
        nodes.insert(info.id.clone(), info);
        self.emit_event(&event);
        true
    }

    /// Deregister a node by ID.
    pub fn deregister(&self, node_id: &str) -> bool {
        let mut nodes = self.nodes.lock();
        if nodes.remove(node_id).is_some() {
            self.emit_event(&ControlEvent::NodeDeregistered {
                node_id: node_id.to_string(),
                timestamp: Utc::now(),
            });
            true
        } else {
            false
        }
    }

    /// Record a heartbeat from a node.
    pub fn heartbeat(&self, node_id: &str) -> bool {
        let mut nodes = self.nodes.lock();
        if let Some(node) = nodes.get_mut(node_id) {
            let old_status = node.status;
            node.last_heartbeat = Utc::now();
            node.missed_heartbeats = 0;
            node.status = NodeStatus::Healthy;

            if old_status != NodeStatus::Healthy {
                self.emit_event(&ControlEvent::NodeStatusChanged {
                    node_id: node_id.to_string(),
                    old_status,
                    new_status: NodeStatus::Healthy,
                    timestamp: Utc::now(),
                });
            }

            self.emit_event(&ControlEvent::NodeHeartbeat {
                node_id: node_id.to_string(),
                timestamp: Utc::now(),
            });
            true
        } else {
            false
        }
    }

    /// List all registered nodes.
    pub fn list_nodes(&self) -> Vec<NodeInfo> {
        self.nodes.lock().values().cloned().collect()
    }

    /// Get a specific node by ID.
    pub fn get_node(&self, node_id: &str) -> Option<NodeInfo> {
        self.nodes.lock().get(node_id).cloned()
    }

    /// Health monitor tick — increment missed heartbeats and update statuses.
    /// Should be called periodically (e.g., every 30s).
    pub fn health_tick(&self) {
        let mut nodes = self.nodes.lock();
        for node in nodes.values_mut() {
            if node.status == NodeStatus::Offline {
                continue;
            }
            node.missed_heartbeats += 1;
            let old_status = node.status;
            let new_status = if node.missed_heartbeats >= self.unreachable_threshold {
                NodeStatus::Unreachable
            } else if node.missed_heartbeats >= self.degraded_threshold {
                NodeStatus::Degraded
            } else {
                NodeStatus::Healthy
            };

            if old_status != new_status {
                node.status = new_status;
            }
        }
    }

    /// Run the health monitor background loop.
    pub async fn health_monitor(self: Arc<Self>, interval_secs: u64) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            interval.tick().await;
            self.health_tick();
        }
    }

    /// Node count.
    pub fn node_count(&self) -> usize {
        self.nodes.lock().len()
    }

    fn emit_event(&self, event: &ControlEvent) {
        if let Ok(json) = serde_json::to_value(event) {
            let _ = self.event_tx.send(json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_control_plane() -> ControlPlane {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        ControlPlane::new(100, tx)
    }

    fn make_node(id: &str) -> NodeInfo {
        NodeInfo {
            id: id.to_string(),
            name: Some(format!("node-{id}")),
            address: Some("127.0.0.1:42618".to_string()),
            version: Some("0.4.3".to_string()),
            capabilities: vec![NodeCapability::Inference, NodeCapability::ToolExecution],
            status: NodeStatus::Healthy,
            registered_at: Utc::now(),
            last_heartbeat: Utc::now(),
            missed_heartbeats: 0,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn test_register_and_list() {
        let cp = make_control_plane();
        assert!(cp.register(make_node("a")));
        assert!(cp.register(make_node("b")));
        assert_eq!(cp.node_count(), 2);
        let nodes = cp.list_nodes();
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn test_deregister() {
        let cp = make_control_plane();
        cp.register(make_node("a"));
        assert!(cp.deregister("a"));
        assert!(!cp.deregister("a")); // already removed
        assert_eq!(cp.node_count(), 0);
    }

    #[test]
    fn test_heartbeat() {
        let cp = make_control_plane();
        cp.register(make_node("a"));
        assert!(cp.heartbeat("a"));
        assert!(!cp.heartbeat("nonexistent"));
    }

    #[test]
    fn test_health_tick_degraded() {
        let cp = make_control_plane();
        cp.register(make_node("a"));
        for _ in 0..3 {
            cp.health_tick();
        }
        let node = cp.get_node("a").unwrap();
        assert_eq!(node.status, NodeStatus::Degraded);
    }

    #[test]
    fn test_health_tick_unreachable() {
        let cp = make_control_plane();
        cp.register(make_node("a"));
        for _ in 0..10 {
            cp.health_tick();
        }
        let node = cp.get_node("a").unwrap();
        assert_eq!(node.status, NodeStatus::Unreachable);
    }

    #[test]
    fn test_heartbeat_resets_status() {
        let cp = make_control_plane();
        cp.register(make_node("a"));
        for _ in 0..5 {
            cp.health_tick();
        }
        assert_eq!(cp.get_node("a").unwrap().status, NodeStatus::Degraded);
        cp.heartbeat("a");
        assert_eq!(cp.get_node("a").unwrap().status, NodeStatus::Healthy);
    }

    #[test]
    fn test_capacity_limit() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let cp = ControlPlane::new(2, tx);
        assert!(cp.register(make_node("a")));
        assert!(cp.register(make_node("b")));
        assert!(!cp.register(make_node("c"))); // at capacity
    }
}
