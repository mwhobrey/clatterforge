// Stands in for `node.exe` invoked as `node index.js -p ... --model ...`,
// cursor-agent's real Windows entry point and real top-level invocation
// shape (confirmed live against the actual installed CLI: its own
// `--help` documents `--print`/`--output-format`/etc. at the top level,
// with no "agent" subcommand word in the real argv - that word names the
// *executable* in cursor-agent's own docs examples, not a CLI argument).
// Args: argv[1] is the index.js path (ignored - this fixture *is* the
// whole fake CLI), argv[2] selects the probe/real invocation. Never
// spends a real cursor-agent credit.
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    thread,
    time::Duration,
};

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() {
    if cfg!(windows) && env::var_os("CONDUCTOR_NATIVE_CURSOR_ENV_TEST").is_some() {
        assert!(env::var_os("MSYSTEM").is_none(), "Cursor must not inherit MSYSTEM");
        assert_eq!(env::var("SHELL").unwrap(), "powershell.exe");
        assert_eq!(env::var("CONDUCTOR_ENV_SENTINEL").unwrap(), "preserved");
    }
    let args: Vec<String> = env::args().skip(2).collect();
    match args.first().map(String::as_str) {
        Some("--version") => println!("2026.10.01-fake"),
        Some("--help") => {
            println!(
                "Options:\n  --print\n  --output-format <format>\n  --model <model>\n  --workspace <path>\n  --sandbox <mode>\n"
            );
        }
        Some("-p") => {
            let workspace = flag(&args, "--workspace").expect("--workspace required");
            // M10.3 Phase 2: records every real invocation to a shared,
            // test-controlled path, when `CONDUCTOR_TEST_INVOCATION_LOG` is
            // set on this process's own environment (inherited from the
            // real `agent-run` CLI process the test spawned, not smuggled
            // per-task) - lets a concurrent-dispatch test prove the losing
            // side never reaches this fixture at all, not merely that its
            // own worktree check would have caught it.
            if let Ok(log) = env::var("CONDUCTOR_TEST_INVOCATION_LOG") {
                let mut file = OpenOptions::new().create(true).append(true).open(log).unwrap();
                writeln!(file, "{}", std::process::id()).unwrap();
            }
            // The mode marker travels inside the prompt text itself (the
            // actual `-p` argument `Agents::execute` builds from the task's
            // objective) - a spawned child's env can't be influenced
            // per-task from the test, but the prompt can.
            let prompt = flag(&args, "-p").unwrap_or_default();
            let mode = if prompt.contains("MODE:progress") {
                "progress"
            } else if prompt.contains("MODE:chatter") {
                "chatter"
            } else if prompt.contains("MODE:out_of_scope") {
                "out_of_scope"
            } else if prompt.contains("MODE:fail") {
                "fail"
            } else if prompt.contains("MODE:hang") {
                "hang"
            } else if prompt.contains("MODE:slow") {
                "slow"
            } else {
                ""
            };
            if mode == "chatter" {
                loop {
                    println!("{}", "x".repeat(8192));
                    thread::sleep(Duration::from_millis(15));
                }
            }
            if mode == "out_of_scope" {
                fs::write(format!("{workspace}/not-allowed.txt"), "sneaky").unwrap();
            } else if mode == "fail" {
                eprintln!("fake agent: simulated failure");
                std::process::exit(1);
            } else if prompt.contains("MODE:write_then_hang") {
                fs::write(format!("{workspace}/src/result.txt"), "partial change before timeout").unwrap();
                loop { thread::sleep(Duration::from_millis(50)); }
            } else if mode == "hang" {
                loop {
                    thread::sleep(Duration::from_millis(50));
                }
            } else {
                // "slow" deliberately holds the workspace/lease long enough
                // for a genuinely concurrent competing dispatch to attempt
                // (and be rejected) mid-attempt, widening the race window a
                // fast fixture could otherwise close before the second
                // `agent-run` process even starts.
                if mode == "slow" {
                    thread::sleep(Duration::from_millis(750));
                }
                if mode == "progress" {
                    let steps = if prompt.contains("MODE:progress_demo") { 1000 } else { 40 };
                    for step in 0..steps {
                        fs::write(format!("{workspace}/src/result.txt"), format!("working step {step}")).unwrap();
                        thread::sleep(Duration::from_millis(100));
                        if prompt.contains("MODE:progress_demo")
                            && env::var_os("CONDUCTOR_TEST_PROGRESS_RELEASE")
                                .is_some_and(|p| std::path::Path::new(&p).exists()) {
                            break;
                        }
                    }
                }
                fs::write(format!("{workspace}/src/result.txt"), "fixed by fake cursor-agent").unwrap();
            }
            println!("{{\"type\":\"result\",\"is_error\":false}}");
        }
        other => {
            eprintln!("unexpected fake-cursor-agent invocation: {other:?}");
            std::process::exit(2);
        }
    }
}
