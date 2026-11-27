//! Diagram 2's plan: the steps of a task and what each one waits for (DECOMPOSE), with a new
//! version whenever a step is dropped (REPLAN). A plan is data. The template comes from the
//! task type's configuration, and `Plan::new` rejects a duplicate step, a dependency on a
//! step that does not exist, and a cycle, so a bad template stops the server at start-up.
//!
//! Whether a step is done is not stored here: it is worked out from stored facts (research
//! results, verified actions), so the plan can never disagree with what happened.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::types::SchemaError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub id: String,
    /// The tool that carries the step out.
    pub tool: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// A person may refuse it and the task goes on without it: an irreversible step the
    /// contract lets a person do instead. Set at FRAME, not in the template.
    #[serde(default)]
    pub skippable: bool,
    #[serde(default)]
    pub skipped: bool,
}

/// The steps, each after the steps it depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Done,
    /// A person refused it and the task goes on without it.
    Skipped,
    /// Everything it depends on is done or skipped.
    Ready,
    Blocked,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Skipped => "skipped",
            Self::Ready => "ready",
            Self::Blocked => "blocked",
        }
    }
}

impl Plan {
    /// Version 1 of a plan: the steps in an order where each comes after what it depends
    /// on, keeping the given order wherever the dependencies allow.
    pub fn new(steps: Vec<PlanStep>) -> Result<Self, SchemaError> {
        let ids: BTreeSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
        if ids.len() != steps.len() {
            return Err(SchemaError("plan_step_ids_not_unique".into()));
        }
        for step in &steps {
            if let Some(missing) = step.depends_on.iter().find(|d| !ids.contains(d.as_str())) {
                return Err(SchemaError(format!(
                    "plan_step_{}_depends_on_unknown_{missing}",
                    step.id
                )));
            }
        }
        // Kahn's algorithm: take the first step, in the given order, whose dependencies
        // are all placed. If none is left to take, the rest form a cycle.
        let mut pending: Vec<PlanStep> = steps;
        let mut placed: BTreeSet<String> = BTreeSet::new();
        let mut ordered = Vec::with_capacity(pending.len());
        while !pending.is_empty() {
            let Some(next) = pending
                .iter()
                .position(|s| s.depends_on.iter().all(|d| placed.contains(d)))
            else {
                let cycle: Vec<&str> = pending.iter().map(|s| s.id.as_str()).collect();
                return Err(SchemaError(format!("plan_has_a_cycle:{}", cycle.join(","))));
            };
            let step = pending.remove(next);
            placed.insert(step.id.clone());
            ordered.push(step);
        }
        Ok(Self {
            version: 1,
            steps: ordered,
        })
    }

    pub fn step(&self, id: &str) -> Option<&PlanStep> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// The step a tool carries out. Tools outside the plan, such as extra reads, are not
    /// ordered by it.
    pub fn for_tool(&self, tool: &str) -> Option<&PlanStep> {
        self.steps.iter().find(|s| s.tool == tool)
    }

    /// Where `step` stands, given the steps that are done.
    pub fn status_of(&self, step: &PlanStep, done: &BTreeSet<String>) -> StepStatus {
        let resolved = |id: &String| done.contains(id) || self.step(id).is_some_and(|s| s.skipped);
        if done.contains(&step.id) {
            StepStatus::Done
        } else if step.skipped {
            StepStatus::Skipped
        } else if step.depends_on.iter().all(resolved) {
            StepStatus::Ready
        } else {
            StepStatus::Blocked
        }
    }

    /// Every step with its status, in plan order.
    pub fn statuses(&self, done: &BTreeSet<String>) -> BTreeMap<&str, StepStatus> {
        self.steps
            .iter()
            .map(|s| (s.id.as_str(), self.status_of(s, done)))
            .collect()
    }

    /// The first step that can run now; `None` when every step is done or skipped.
    pub fn next_ready(&self, done: &BTreeSet<String>) -> Option<&PlanStep> {
        self.steps
            .iter()
            .find(|s| self.status_of(s, done) == StepStatus::Ready)
    }

    /// The steps `step` still waits for.
    pub fn waiting_for(&self, step: &PlanStep, done: &BTreeSet<String>) -> Vec<String> {
        step.depends_on
            .iter()
            .filter(|d| !done.contains(*d) && !self.step(d).is_some_and(|s| s.skipped))
            .cloned()
            .collect()
    }

    /// REPLAN: the next version, without `id`. Only a skippable step can be dropped.
    pub fn skip(&self, id: &str) -> Result<Self, SchemaError> {
        let mut next = self.clone();
        let step = next
            .steps
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| SchemaError(format!("plan_has_no_step:{id}")))?;
        if !step.skippable {
            return Err(SchemaError(format!("plan_step_not_skippable:{id}")));
        }
        step.skipped = true;
        next.version += 1;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn step(id: &str, depends_on: &[&str]) -> PlanStep {
        PlanStep {
            id: id.into(),
            tool: format!("tool_{id}"),
            depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
            skippable: false,
            skipped: false,
        }
    }

    fn rebalance() -> Plan {
        let mut orders = step("orders", &["draft"]);
        orders.skippable = true;
        Plan::new(vec![
            step("research", &[]),
            step("draft", &["research"]),
            orders,
        ])
        .unwrap()
    }

    fn done(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn steps_are_put_after_what_they_depend_on() {
        let plan = Plan::new(vec![
            step("orders", &["draft"]),
            step("draft", &["research"]),
            step("research", &[]),
        ])
        .unwrap();
        let order: Vec<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(order, ["research", "draft", "orders"]);
    }

    #[test]
    fn bad_templates_are_refused() {
        let cycle = Plan::new(vec![step("a", &["b"]), step("b", &["a"])]);
        assert_eq!(cycle.unwrap_err().0, "plan_has_a_cycle:a,b");
        let unknown = Plan::new(vec![step("a", &["nowhere"])]);
        assert_eq!(
            unknown.unwrap_err().0,
            "plan_step_a_depends_on_unknown_nowhere"
        );
        let twice = Plan::new(vec![step("a", &[]), step("a", &[])]);
        assert_eq!(twice.unwrap_err().0, "plan_step_ids_not_unique");
    }

    #[test]
    fn each_step_waits_for_what_it_depends_on() {
        let plan = rebalance();
        let none = done(&[]);
        assert_eq!(plan.next_ready(&none).unwrap().id, "research");
        let orders = plan.step("orders").unwrap();
        assert_eq!(plan.status_of(orders, &none), StepStatus::Blocked);
        assert_eq!(plan.waiting_for(orders, &none), ["draft"]);
        let drafted = done(&["research", "draft"]);
        assert_eq!(plan.next_ready(&drafted).unwrap().id, "orders");
        assert!(
            plan.next_ready(&done(&["research", "draft", "orders"]))
                .is_none()
        );
    }

    #[test]
    fn replanning_drops_a_step_in_a_new_version() {
        let plan = rebalance();
        assert_eq!(
            plan.skip("draft").unwrap_err().0,
            "plan_step_not_skippable:draft"
        );
        let next = plan.skip("orders").unwrap();
        assert_eq!(next.version, 2);
        let drafted = done(&["research", "draft"]);
        let orders = next.step("orders").unwrap();
        assert_eq!(next.status_of(orders, &drafted), StepStatus::Skipped);
        assert!(next.next_ready(&drafted).is_none());
        assert_eq!(plan.version, 1, "the earlier version is unchanged");
    }

    proptest! {
        /// Any graph whose edges point from later to earlier steps is acyclic: it is
        /// accepted, and every step comes after its dependencies.
        #[test]
        fn acyclic_graphs_are_ordered(edges in prop::collection::vec((0usize..8, 0usize..8), 0..20)) {
            let mut steps: Vec<PlanStep> = (0..8).map(|i| step(&format!("s{i}"), &[])).collect();
            for (a, b) in edges {
                let (later, earlier) = (a.max(b), a.min(b));
                if later != earlier {
                    steps[later].depends_on.push(format!("s{earlier}"));
                }
            }
            for s in &mut steps {
                s.depends_on.sort();
                s.depends_on.dedup();
            }
            steps.reverse();
            let plan = Plan::new(steps).unwrap();
            for (i, s) in plan.steps.iter().enumerate() {
                for d in &s.depends_on {
                    let at = plan.steps.iter().position(|x| &x.id == d).unwrap();
                    prop_assert!(at < i);
                }
            }
        }

        /// Adding one edge back from an earlier step to a later one in a chain always
        /// makes a cycle, and it is always refused.
        #[test]
        fn a_back_edge_makes_a_cycle_that_is_refused(len in 2usize..8, from in 0usize..8, to in 0usize..8) {
            let (from, to) = (from % len, to % len);
            prop_assume!(from < to);
            let mut steps: Vec<PlanStep> = (0..len)
                .map(|i| if i == 0 { step("s0", &[]) } else { step(&format!("s{i}"), &[&format!("s{}", i - 1)]) })
                .collect();
            steps[from].depends_on.push(format!("s{to}"));
            let refused = Plan::new(steps).unwrap_err();
            prop_assert!(refused.0.starts_with("plan_has_a_cycle:"));
        }
    }
}
