//! End-to-end correctness of the fixed scenario set.

use hive_eval::{run_once, Scenario};

#[test]
fn single_scenario_reports_ok() {
    let record = run_once(Scenario::Single, "test", 1);
    assert!(record.ok, "s1-single failed: {record:?}");
    assert_eq!(record.scenario, "s1-single");
}

#[test]
fn dag_scenario_reports_ok() {
    let record = run_once(Scenario::Dag, "test", 1);
    assert!(record.ok, "s2-dag failed: {record:?}");
    assert_eq!(record.nodes, 10);
    assert_eq!(record.workers, 4);
}

#[test]
fn handoff_scenario_reports_ok() {
    let record = run_once(Scenario::Handoff, "test", 1);
    assert!(record.ok, "s3-handoff failed: {record:?}");
}

#[test]
fn retry_scenario_reports_ok() {
    let record = run_once(Scenario::Retry, "test", 1);
    assert!(record.ok, "s4-retry failed: {record:?}");
}

#[test]
fn cancel_scenario_reports_ok() {
    let record = run_once(Scenario::Cancel, "test", 1);
    assert!(record.ok, "s6-cancel failed: {record:?}");
}

#[test]
fn budget_scenario_reports_ok() {
    let record = run_once(Scenario::Budget, "test", 1);
    assert!(record.ok, "s5-budget failed: {record:?}");
    assert_eq!(record.scenario, "s5-budget");
    assert_eq!(record.nodes, 2);
}
