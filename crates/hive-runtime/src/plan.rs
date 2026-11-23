use std::collections::{HashMap, VecDeque};

use hive_types::{MissionId, NodeId, RunId};
use thiserror::Error;

/// Static description of a single DAG node.
///
/// The deterministic scheduler does not interpret `spec`; it is handed to the
/// [`crate::NodeExecutor`] when the node is started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeSpec {
    pub role: String,
    pub model: String,
    pub objective: String,
}

impl NodeSpec {
    pub fn new(role: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            model: model.into(),
            objective: String::new(),
        }
    }

    pub fn with_objective(mut self, objective: impl Into<String>) -> Self {
        self.objective = objective.into();
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanNode {
    pub node_id: NodeId,
    pub deps: Vec<NodeId>,
    pub spec: NodeSpec,
    /// Extra executions after the first one; the node gets at most
    /// `retry_budget + 1` attempts (PROTOCOL.md section 2).
    pub retry_budget: u32,
}

/// The immutable DAG describing one Run's execution plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plan {
    /// Stable identity of the plan. There is no dedicated `PlanId` prefix in
    /// PROTOCOL.md, so this is kept as an opaque string.
    pub plan_id: String,
    pub mission_id: MissionId,
    pub run_id: RunId,
    pub nodes: Vec<PlanNode>,
}

/// Validated, topologically ordered view of a [`Plan`].
#[derive(Clone, Debug)]
pub struct Topology {
    pub mission_id: MissionId,
    pub run_id: RunId,
    order: Vec<NodeId>,
    deps: HashMap<NodeId, Vec<NodeId>>,
    dependents: HashMap<NodeId, Vec<NodeId>>,
    nodes: HashMap<NodeId, PlanNode>,
}

impl Topology {
    pub fn order(&self) -> &[NodeId] {
        &self.order
    }

    pub fn node(&self, node_id: NodeId) -> Option<&PlanNode> {
        self.nodes.get(&node_id)
    }

    pub fn deps_of(&self, node_id: NodeId) -> &[NodeId] {
        self.deps.get(&node_id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn dependents_of(&self, node_id: NodeId) -> &[NodeId] {
        self.dependents
            .get(&node_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum PlanError {
    #[error("plan has no nodes")]
    EmptyPlan,
    #[error("duplicate node id `{0}`")]
    DuplicateNode(NodeId),
    #[error("node `{0}` depends on itself")]
    SelfDependency(NodeId),
    #[error("node `{node}` depends on missing node `{dep}`")]
    MissingDependency { node: NodeId, dep: NodeId },
    #[error("plan contains a dependency cycle")]
    Cycle,
}

/// Validate a plan and return its topological order.
///
/// Rejects empty plans, duplicate node ids, self-dependencies, missing
/// dependencies and dependency cycles. The topological order is deterministic
/// for a given `nodes` input order.
pub fn validate_plan(plan: &Plan) -> Result<Topology, PlanError> {
    if plan.nodes.is_empty() {
        return Err(PlanError::EmptyPlan);
    }

    let mut nodes = HashMap::with_capacity(plan.nodes.len());
    for node in &plan.nodes {
        if nodes.insert(node.node_id, node.clone()).is_some() {
            return Err(PlanError::DuplicateNode(node.node_id));
        }
    }

    for node in &plan.nodes {
        for dep in &node.deps {
            if *dep == node.node_id {
                return Err(PlanError::SelfDependency(node.node_id));
            }
            if !nodes.contains_key(dep) {
                return Err(PlanError::MissingDependency {
                    node: node.node_id,
                    dep: *dep,
                });
            }
        }
    }

    let mut deps = HashMap::with_capacity(plan.nodes.len());
    let mut dependents: HashMap<NodeId, Vec<NodeId>> = HashMap::with_capacity(plan.nodes.len());
    let mut indegree = HashMap::with_capacity(plan.nodes.len());
    for node in &plan.nodes {
        deps.insert(node.node_id, node.deps.clone());
        dependents.entry(node.node_id).or_default();
        indegree.insert(node.node_id, node.deps.len());
    }
    for node in &plan.nodes {
        for dep in &node.deps {
            dependents.entry(*dep).or_default().push(node.node_id);
        }
    }

    let mut queue: VecDeque<NodeId> = plan
        .nodes
        .iter()
        .filter(|node| node.deps.is_empty())
        .map(|node| node.node_id)
        .collect();
    let mut order = Vec::with_capacity(plan.nodes.len());
    while let Some(node_id) = queue.pop_front() {
        order.push(node_id);
        for dependent in dependents.get(&node_id).map(Vec::as_slice).unwrap_or(&[]) {
            let remaining = indegree.get_mut(dependent).expect("dependent has indegree");
            *remaining -= 1;
            if *remaining == 0 {
                queue.push_back(*dependent);
            }
        }
    }

    if order.len() != plan.nodes.len() {
        return Err(PlanError::Cycle);
    }

    Ok(Topology {
        mission_id: plan.mission_id,
        run_id: plan.run_id,
        order,
        deps,
        dependents,
        nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: NodeId, deps: &[NodeId]) -> PlanNode {
        PlanNode {
            node_id: id,
            deps: deps.to_vec(),
            spec: NodeSpec::new("worker", "auto/auto"),
            retry_budget: 3,
        }
    }

    fn plan(nodes: Vec<PlanNode>) -> Plan {
        Plan {
            plan_id: "plan-1".to_owned(),
            mission_id: MissionId::new(),
            run_id: RunId::new(),
            nodes,
        }
    }

    #[test]
    fn empty_plan_is_rejected() {
        assert_eq!(
            validate_plan(&plan(vec![])).unwrap_err(),
            PlanError::EmptyPlan
        );
    }

    #[test]
    fn duplicate_node_is_rejected() {
        let id = NodeId::new();
        let error = validate_plan(&plan(vec![node(id, &[]), node(id, &[])])).unwrap_err();
        assert_eq!(error, PlanError::DuplicateNode(id));
    }

    #[test]
    fn self_dependency_is_rejected() {
        let id = NodeId::new();
        let error = validate_plan(&plan(vec![node(id, &[id])])).unwrap_err();
        assert_eq!(error, PlanError::SelfDependency(id));
    }

    #[test]
    fn missing_dependency_is_rejected() {
        let id = NodeId::new();
        let missing = NodeId::new();
        let error = validate_plan(&plan(vec![node(id, &[missing])])).unwrap_err();
        assert_eq!(
            error,
            PlanError::MissingDependency {
                node: id,
                dep: missing
            }
        );
    }

    #[test]
    fn cycle_is_rejected() {
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let error =
            validate_plan(&plan(vec![node(a, &[c]), node(b, &[a]), node(c, &[b])])).unwrap_err();
        assert_eq!(error, PlanError::Cycle);
    }

    #[test]
    fn diamond_topology_orders_deps_before_dependents() {
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let d = NodeId::new();
        let topology = validate_plan(&plan(vec![
            node(a, &[]),
            node(b, &[a]),
            node(c, &[a]),
            node(d, &[b, c]),
        ]))
        .unwrap();
        let order = topology.order();
        let index = |id: NodeId| order.iter().position(|x| *x == id).unwrap();
        assert!(index(a) < index(b));
        assert!(index(a) < index(c));
        assert!(index(b) < index(d));
        assert!(index(c) < index(d));
        assert_eq!(topology.deps_of(d).len(), 2);
        assert_eq!(topology.dependents_of(a).len(), 2);
    }
}
