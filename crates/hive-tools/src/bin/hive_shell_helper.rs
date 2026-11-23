//! Minimal helper executable used by `hive-tools` shell integration tests.
//! It is deliberately tiny and depends only on the standard library.

use std::env;
use std::fs;
use std::process;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("echo") => {
            for argument in args.iter().skip(1) {
                println!("{argument}");
            }
        }
        Some("err") => {
            for argument in args.iter().skip(1) {
                eprintln!("{argument}");
            }
        }
        Some("exit") => {
            let code: i32 = args
                .get(1)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            process::exit(code);
        }
        Some("overflow") => {
            for _ in 0..200_000 {
                print!("x");
            }
        }
        Some("sleep") => {
            let millis: u64 = args
                .get(1)
                .and_then(|value| value.parse().ok())
                .unwrap_or(1_000);
            thread::sleep(Duration::from_millis(millis));
            if let Some(marker) = args.get(2) {
                let _ = fs::write(marker, "finished");
            }
        }
        Some("spawn-tree") => {
            let millis: u64 = args
                .get(1)
                .and_then(|value| value.parse().ok())
                .unwrap_or(5_000);
            let marker = args.get(2).cloned();
            // Give the caller a moment to attach this process to its Job
            // Object so the child inherits it and dies with the tree.
            thread::sleep(Duration::from_millis(200));
            let mut command = process::Command::new(env::current_exe().expect("helper exe"));
            command.arg("sleep").arg(millis.to_string());
            if let Some(marker) = &marker {
                command.arg(marker);
            }
            let mut child = command.spawn().expect("spawn child");
            let _ = child.wait();
        }
        _ => {
            eprintln!("unknown hive_shell_helper mode");
            process::exit(64);
        }
    }
}
