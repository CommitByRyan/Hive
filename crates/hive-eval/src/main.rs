//! CLI entry for the evaluation harness.
//!
//! ```text
//! hive-eval scenario <name> --runs <n> --out <path.jsonl>
//! hive-eval kill-worker <db> <node_id> <marker>   # internal, for kill tests
//! ```

use std::io::Write;
use std::path::Path;
use std::str::FromStr;

use hive_eval::{harness, run_once, Scenario};
use hive_store::{LeaseGuard, Store};
use hive_types::{
    CancellationToken, EventId, EventInput, EventKind, LeaseToken, NodeId, NodeState,
};
use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: hive-eval scenario <name> [--runs <n>] [--out <path.jsonl>]");
        std::process::exit(2);
    }

    match args[0].as_str() {
        "scenario" => run_scenario_cli(&args[1..]),
        "kill-worker" => run_kill_worker(&args[1..]),
        other => {
            eprintln!("unknown subcommand `{other}`");
            std::process::exit(2);
        }
    }
}

fn run_scenario_cli(args: &[String]) {
    let name = match args.first() {
        Some(name) => name.as_str(),
        None => {
            eprintln!("scenario name is required");
            std::process::exit(2);
        }
    };
    let scenario = Scenario::parse(name).unwrap_or_else(|| {
        eprintln!("unknown scenario `{name}`; known: s1-single s2-dag s3-handoff s4-retry s5-budget s6-cancel");
        std::process::exit(2);
    });

    let mut runs = 1u32;
    let mut out: Option<String> = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--runs" => {
                index += 1;
                runs = args
                    .get(index)
                    .and_then(|value| value.parse().ok())
                    .filter(|value| *value > 0)
                    .unwrap_or_else(|| {
                        eprintln!("--runs requires a positive integer");
                        std::process::exit(2);
                    });
            }
            "--out" => {
                index += 1;
                out = args.get(index).cloned();
                if out.is_none() {
                    eprintln!("--out requires a path");
                    std::process::exit(2);
                }
            }
            other => {
                eprintln!("unknown option `{other}`");
                std::process::exit(2);
            }
        }
        index += 1;
    }

    let commit = harness::current_commit();
    let mut writer: Box<dyn Write> = match &out {
        Some(path) => Box::new(std::fs::File::create(path).unwrap_or_else(|error| {
            eprintln!("cannot open `{path}`: {error}");
            std::process::exit(1);
        })),
        None => Box::new(std::io::stdout()),
    };

    for run_index in 1..=runs {
        let record = run_once(scenario, &commit, run_index);
        if record.blocked.is_some() {
            eprintln!(
                "{}: blocked ({})",
                scenario.name(),
                record.blocked.unwrap_or("unknown")
            );
        }
        record.write_jsonl(&mut writer).unwrap_or_else(|error| {
            eprintln!("cannot write output: {error}");
            std::process::exit(1);
        });
    }
}

/// Internal helper for the strong-kill recovery test: lease a ready node,
/// move it to `running` (emitting `node.started`), signal readiness via a
/// marker file and then block forever until killed.
fn run_kill_worker(args: &[String]) {
    if args.len() < 3 {
        eprintln!("usage: hive-eval kill-worker <db> <node_id> <marker>");
        std::process::exit(2);
    }
    let db_path = Path::new(&args[0]);
    let node_id = NodeId::from_str(&args[1]).unwrap_or_else(|error| {
        eprintln!("invalid node id: {error}");
        std::process::exit(2);
    });
    let marker = Path::new(&args[2]);

    let mut store = Store::open(db_path).unwrap_or_else(|error| {
        eprintln!("cannot open db: {error}");
        std::process::exit(1);
    });
    let node = store.get_node(node_id).unwrap_or_else(|error| {
        eprintln!("cannot read node: {error}");
        std::process::exit(1);
    });
    let node = node.unwrap_or_else(|| {
        eprintln!("node {node_id} not found");
        std::process::exit(1);
    });

    let now = harness::now_ms();
    let token = LeaseToken::generate();
    let leased = store
        .lease_node(node_id, &token, "hive-eval-kill-worker", now + 60_000, now)
        .unwrap_or_else(|error| {
            eprintln!("cannot lease node: {error}");
            std::process::exit(1);
        });
    let guard = LeaseGuard {
        attempt_no: leased.attempt_no,
        lease_token: token,
    };
    let started = EventInput {
        event_id: EventId::new(),
        kind: EventKind::NodeStarted,
        mission_id: node.mission_id,
        run_id: Some(node.run_id),
        node_id: Some(node_id),
        session_id: None,
        attempt_no: Some(guard.attempt_no),
        trace_id: None,
        payload: json!({}),
    };
    store
        .transition_node_cas(
            node_id,
            &guard,
            NodeState::Running,
            &started,
            harness::now_ms(),
        )
        .unwrap_or_else(|error| {
            eprintln!("cannot start node: {error}");
            std::process::exit(1);
        });
    std::fs::write(marker, "started").unwrap_or_else(|error| {
        eprintln!("cannot write marker: {error}");
        std::process::exit(1);
    });

    let _token = CancellationToken::new();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
