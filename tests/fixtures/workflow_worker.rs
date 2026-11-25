use std::{env,fs,path::PathBuf,process,thread,time::Duration};
fn main() {
    let args:Vec<_>=env::args().skip(1).collect();
    if args.iter().any(|v|v=="--version") {println!("workflow fixture 1");return;}
    if args.iter().any(|v|v=="--help") {println!("--print --output-format --model --workspace --sandbox --json --ephemeral --ignore-user-config --no-daemon --ask-for-approval --color --mode --dangerously-skip-permissions");return;}
    if args.iter().any(|v|v=="--list-models") {
        if let Some(index_js) = args.first().map(PathBuf::from) {
            if let Some(dir) = index_js.parent() {
                if let Ok(custom) = fs::read_to_string(dir.join("live_models.txt")) {
                    print!("{custom}");
                    return;
                }
            }
        }
        println!("Available models\n\nfixture - Fixture\nauto - Auto\ncomposer-2.5-fast - Composer 2.5 Fast\ngpt-4o-mini - GPT-4o Mini\ncomposer-2.5 - Composer 2.5\nclaude-3-7-sonnet - Claude 3.7 Sonnet\nsonnet-4-thinking - Claude 4 Sonnet Thinking\nclaude-opus-5-thinking-high - Claude 5 Opus Thinking\ngpt-5 - GPT-5");
        return;
    }
    if args.iter().any(|v|v=="--verify-hang") {
        eprintln!("verifier compiling workspace...");
        loop {thread::sleep(Duration::from_millis(20));}
    }
    if args.iter().any(|v|v=="--verify") {
        assert!(env::var_os("CONDUCTOR_AGENTLET_CREDENTIAL_SENTINEL").is_none());
        let ok=fs::read_to_string("a.txt").unwrap().starts_with("fixed-a") && fs::read_to_string("b.txt").unwrap()=="fixed-b";
        eprintln!("independent acceptance: a and b must both be fixed");
        process::exit(if ok {0}else{3});
    }
    let is_codex = args.iter().any(|v| v == "exec");
    let ws_flag = if is_codex { "--cd" } else { "--workspace" };
    let workspace = args
        .iter()
        .position(|v| v == ws_flag)
        .map(|pos| PathBuf::from(&args[pos + 1]))
        .unwrap_or_else(|| env::current_dir().unwrap());
    let model = args.iter().position(|v|v=="--model").map(|i| args[i+1].as_str()).unwrap_or("unknown");
    let prompt = if is_codex {
        args.last().unwrap()
    } else {
        &args[args.iter().position(|v|v=="-p").unwrap()+1]
    };
    let hang_dirty_marker = format!("HANG_DIRTY_{model}");
    if prompt.contains(&hang_dirty_marker) {
        fs::write(workspace.join("a.txt"), "partial-unverified-edit").unwrap();
        loop {
            thread::sleep(Duration::from_millis(20));
        }
    }
    let quota_marker = format!("QUOTA_FAIL_{model}");
    let long_jsonl_marker = format!("QUOTA_FAIL_LONG_JSONL_{model}");
    if prompt.contains(&long_jsonl_marker) {
        let filler = "x".repeat(6000);
        if is_codex {
            println!("{{\"type\":\"thread.started\",\"thread_id\":\"t-1\",\"pad\":\"{filler}\"}}");
            println!("{{\"type\":\"turn.started\"}}");
            println!("{{\"type\":\"error\",\"message\":\"You've hit your usage_limit_reached. Upgrade to Plus to continue.\"}}");
            println!("{{\"type\":\"turn.failed\",\"error\":{{\"message\":\"usage_limit_reached\"}}}}");
        } else {
            println!("{{\"type\":\"progress\",\"pad\":\"{filler}\"}}");
            eprintln!("ConnectError: [resource_exhausted] You've hit your usage limit. Upgrade your plan.");
        }
        process::exit(1);
    }
    if prompt.contains(&quota_marker) {
        if is_codex {
            eprintln!("OpenAI Codex error: You've hit your usage limit (status 429). Upgrade to Pro.");
        } else if args.iter().any(|v| v == "--workspace") {
            eprintln!("ConnectError: [resource_exhausted] You've hit your usage limit. Upgrade your plan.");
        } else {
            eprintln!(
                "error: Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 148h9m26s.\nAGY_ERROR: {{\"short_error\":\"RESOURCE_EXHAUSTED (code 429): Individual quota reached.\",\"status\":\"RESOURCE_EXHAUSTED\",\"error_code\":429,\"code_kind\":\"http\",\"retryable\":true}}"
            );
        }
        process::exit(1);
    }
    let agentlet=prompt.contains("AGENTLET_CONTRACT_V1");
    if agentlet {assert!(env::var_os("CONDUCTOR_AGENTLET_CREDENTIAL_SENTINEL").is_none());}
    let objective=if agentlet {prompt.split("\"objective\":\"").nth(1).unwrap().split('"').next().unwrap()} else {prompt.as_str()};
    if prompt.contains("HANG_WORKER") {loop {thread::sleep(Duration::from_millis(20));}}
    if objective.contains("STAGE_SCOPED_WORK") {
        if objective.contains("editable files: a.txt") {
            fs::write(workspace.join("a.txt"), "fixed-a").unwrap();
        }
        if objective.contains("editable files: b.txt") {
            fs::write(workspace.join("b.txt"), "fixed-b").unwrap();
        }
    }
    if objective.contains("WORK_BOTH") {
        fs::write(workspace.join("a.txt"),"fixed-a").unwrap();
        fs::write(workspace.join("b.txt"),"fixed-b").unwrap();
    }
    if objective.contains("ENV_REPORT") {
        let seen = |name: &str| env::var_os(name).is_some();
        fs::write(workspace.join("a.txt"), format!("fixed-a secret={} listed={}", seen("CLATTERFORGE_TEST_AMBIENT_SECRET"), seen("CLATTERFORGE_TEST_LISTED"))).unwrap();
    }
    if objective.contains("WORK_IF_FRONTIER") {
        fs::write(workspace.join("a.txt"),"fixed-a").unwrap();
        if model.contains("thinking") || model.contains("opus") || model == "gpt-5" || model == "o3" {
            fs::write(workspace.join("b.txt"),"fixed-b").unwrap();
        }
    }
    if objective.starts_with("WORK_A") {fs::write(workspace.join("a.txt"),"fixed-a").unwrap();}
    if objective.starts_with("WORK_A_CHANGED") {fs::write(workspace.join("a.txt"),"fixed-a-modified").unwrap();}
    if prompt.starts_with("WORK_A_LARGE") {fs::write(workspace.join("a.txt"),format!("fixed-a{}", "candidate-body-marker".repeat(8000))).unwrap();}
    if prompt.starts_with("WORK_A_EOL") {fs::write(workspace.join("a.txt"),"fixed-a\r\n").unwrap();}
    if objective.starts_with("WORK_B") {
        assert!(fs::read_to_string(workspace.join("a.txt")).unwrap().starts_with("fixed-a"),"missing candidate handoff");
        fs::write(workspace.join("b.txt"),"fixed-b").unwrap();
        if prompt.contains("EDIT_INHERITED") {fs::write(workspace.join("a.txt"),"tampered").unwrap();}
        if prompt.contains("EDIT_EOL") {fs::write(workspace.join("a.txt"),"fixed-a\n").unwrap();}
    }
    if agentlet {
        if objective.contains("REVIEW_TAMPER") {fs::write(workspace.join("a.txt"),"tampered").unwrap();}
        if objective.contains("REVIEW_OK") {
            assert!(fs::read_to_string(workspace.join("a.txt")).unwrap().starts_with("fixed-a"));
            assert_eq!(fs::read_to_string(workspace.join("b.txt")).unwrap(),"fixed-b");
            assert!(prompt.contains("agentlet_result"),"structured prior result not handed off");
        }
        let report=if objective.contains("BAD_REPORT") {"{\"schema_version\":1,\"outcome\":\"completed\",\"summary\":\"fake\",\"findings\":[],\"execute\":\"unapproved\"}".to_string()}
        else if objective.contains("BLOCKED") {"{\"schema_version\":1,\"outcome\":\"blocked\",\"summary\":\"Needs operator attention\",\"findings\":[{\"severity\":\"error\",\"message\":\"Do not finish without a clean review\"}],\"next_objective\":\"Investigate the reported problem\"}".to_string()}
        else {"{\"schema_version\":1,\"outcome\":\"completed\",\"summary\":\"Specialist inspected its scope\",\"findings\":[],\"next_objective\":null}".to_string()};
        let is_agy = !is_codex && args.iter().any(|v| v == "--mode");
        if is_codex {
            println!("{{\"type\":\"thread.started\",\"thread_id\":\"t-fixture\"}}");
            println!("{{\"type\":\"turn.started\"}}");
            println!("{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":{:?}}}}}",report);
            println!("{{\"type\":\"turn.completed\"}}");
        } else if is_agy {
            println!("{{\"status\":\"SUCCESS\",\"response\":{:?}}}",report);
        } else {
            println!("{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":{:?}}}",report);
        }
        return;
    }
    if model == "fixture" {
        println!("Worker evidence: inspected the task and changed its permitted file.");
    } else {
        println!("Worker evidence (cli_model={model}): inspected the task and changed its permitted file.");
    }
}
