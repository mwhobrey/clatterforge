// M10.4 Phase 4: stands in for the real Codex CLI invoked through
// `codex::CodexPolicy::command_for_prompt` (the `Agents` engine's coding
// adapter) - never the Store engine's `codex-exec-v1` exact-content
// shape. Spawned directly as `fake.exe <args...>`, with no extra
// leading positional the way cursor-agent's `node.exe <index.js> ...`
// has, so argv parsing here skips only argv[0] itself.
//
// Mode markers travel inside the trailing prompt argument itself (the
// free-form coding objective `Agents::execute` builds from the task's
// objective/acceptance), exactly like `fake_cursor_agent.rs`'s own
// `MODE:` convention - a spawned child's env can't be influenced
// per-task from the test, but the prompt can.
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
    let args: Vec<String> = env::args().skip(1).collect();
    if args == ["--version"] {
        println!("codex-cli fake-fixture");
        return;
    }
    if args == ["--help"] {
        println!("Options:\n  --no-daemon\n  --ask-for-approval <policy>\n");
        return;
    }
    if args == ["exec", "--help"] {
        println!(
            "Options:\n  --json\n  --ephemeral\n  --ignore-user-config\n  --sandbox <mode>\n  --model <model>\n"
        );
        return;
    }
    assert!(args.iter().any(|a| a == "--no-daemon"));
    assert!(args.iter().any(|a| a == "--ignore-user-config"));
    assert!(args.iter().any(|a| a == "windows.sandbox=\"mxc\""));
    // M10.4 Phase 4 concurrent-dispatch test: records every real
    // invocation to a shared, test-controlled path, when set on this
    // process's own environment (inherited from the real `agent-run`
    // CLI process the test spawned, not smuggled per-task).
    if let Ok(log) = env::var("CONDUCTOR_TEST_INVOCATION_LOG") {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        writeln!(file, "{}", std::process::id()).unwrap();
    }
    let workspace = flag(&args, "--cd").expect("--cd required");
    let prompt = args.last().cloned().unwrap_or_default();
    let mode = if prompt.contains("MODE:out_of_scope") {
        "out_of_scope"
    } else if prompt.contains("MODE:fail") {
        "fail"
    } else if prompt.contains("MODE:hang") {
        "hang"
    } else if prompt.contains("MODE:malformed") {
        "malformed"
    } else if prompt.contains("MODE:wrong") {
        "wrong"
    } else if prompt.contains("MODE:slow") {
        "slow"
    } else {
        ""
    };
    match mode {
        "out_of_scope" => {
            fs::write(format!("{workspace}/not-allowed.txt"), "sneaky").unwrap();
            println!("{{\"type\":\"thread.started\",\"thread_id\":\"fake\"}}");
            println!("{{\"type\":\"turn.started\"}}");
            println!(
                "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"done\"}}}}"
            );
            println!("{{\"type\":\"turn.completed\"}}");
        }
        "fail" => {
            eprintln!("fake codex: simulated failure");
            std::process::exit(1);
        }
        "hang" => loop {
            thread::sleep(Duration::from_millis(50));
        },
        "malformed" => {
            println!("not even json");
        }
        "wrong" => {
            // Stays in scope (edits only the allowed file) but produces
            // content the independent verifier does not accept - proves
            // verification is a genuine, separate check, not a rubber
            // stamp on "the agent's process exited 0".
            fs::write(format!("{workspace}/src/result.txt"), "still wrong").unwrap();
            println!("{{\"type\":\"thread.started\",\"thread_id\":\"fake\"}}");
            println!("{{\"type\":\"turn.started\"}}");
            println!(
                "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"I believe this is fixed\"}}}}"
            );
            println!("{{\"type\":\"turn.completed\"}}");
        }
        _ => {
            if mode == "slow" {
                thread::sleep(Duration::from_millis(750));
            }
            // Matches the literal string `tests/agents.rs`'s shared
            // `fixture_repo()` writes into `verification.txt` - reused
            // unchanged for both backends' tests rather than duplicating
            // fixture setup per backend.
            fs::write(format!("{workspace}/src/result.txt"), "fixed by fake cursor-agent").unwrap();
            println!("{{\"type\":\"thread.started\",\"thread_id\":\"fake\"}}");
            println!("{{\"type\":\"turn.started\"}}");
            println!(
                "{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"fixed it\"}}}}"
            );
            println!("{{\"type\":\"turn.completed\"}}");
        }
    }
}
