// A protocol fixture, compiled into an isolated temporary directory by integration tests.
use std::{env, fs, io::{self, Read, Write}, process::Command, thread, time::Duration};
const CONTENTS: &str = "def add(a, b):\n    return a + b\n";
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") { println!("codex-cli fixture-contract"); return; }
    if args.iter().any(|a| a == "--help") {
        println!("--no-daemon --ask-for-approval --json --ephemeral --ignore-user-config --sandbox --model"); return;
    }
    if args.first().is_some_and(|a| a == "--descendant") { loop { thread::sleep(Duration::from_secs(1)); } }
    if args.first().is_some_and(|a| a == "--check") {
        if args.iter().any(|a| a == "hang") { loop { thread::sleep(Duration::from_secs(1)); } }
        if args.iter().any(|a| a == "mutate") { fs::write("result.py", "changed during verification").unwrap(); return; }
        if args.iter().any(|a| a == "fail") { std::process::exit(9); }
        let passed = fs::read_to_string("result.py").ok().as_deref() == Some(CONTENTS);
        println!("independent fixture check: {passed}");
        std::process::exit(if passed { 0 } else { 7 });
    }
    let model = args.windows(2).find(|a| a[0] == "--model").unwrap()[1].clone();
    assert!(args.iter().any(|a| a == "--no-daemon"));
    assert!(args.iter().any(|a| a == "--ignore-user-config"));
    assert!(args.iter().any(|a| a == "windows.sandbox=\"mxc\""));
    assert!(env::var_os("CODEX_API_KEY").is_none());
    assert!(env::var_os("OPENAI_API_KEY").is_none());
    // M10.3: a real live Codex run failed because its own MXC sandbox mode
    // requires this platform value and the launcher's allowlist omitted it.
    assert!(env::var_os("SystemDrive").is_some());
    let mut input = String::new(); io::stdin().read_to_string(&mut input).unwrap();
    assert!(input.contains("def add") && input.contains("Repository context"));
    if model == "hang" || model == "descendant" {
        if model == "descendant" {
            let mut child = Command::new(env::current_exe().unwrap()).arg("--descendant")
                .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
            fs::write("descendant.pid", child.id().to_string()).unwrap();
            let _ = child.wait();
        }
        loop { thread::sleep(Duration::from_secs(1)); }
    }
    if model != "no-write" { fs::write("result.py", if model == "wrong" { "wrong" } else { CONTENTS }).unwrap(); }
    if model == "tracked" { fs::write("README.md", "unauthorized change").unwrap(); }
    if model == "extra" { fs::write("extra.txt", "unauthorized").unwrap(); }
    println!("{{\"type\":\"thread.started\",\"thread_id\":\"fixture-thread\"}}");
    println!("{{\"type\":\"turn.started\"}}");
    if model == "flood" {
        for _ in 0..50000 { println!("{{\"type\":\"item.updated\",\"item\":{{\"type\":\"reasoning\",\"text\":\"fixture output flood\"}}}}"); }
    }
    if model == "malformed" { println!("not JSON"); return; }
    if model == "oversize" {
        let mut out = io::stdout().lock();
        out.write_all(b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"").unwrap();
        out.write_all(&vec![b'x'; 300000]).unwrap(); out.write_all(b"\"}}\n").unwrap();
    } else if model != "empty" {
        println!("{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"tests passed (agent claim only)\"}}}}");
    }
    if model == "failed-event" { println!("{{\"type\":\"turn.failed\"}}"); return; }
    if model != "incomplete" { println!("{{\"type\":\"turn.completed\",\"usage\":{{\"output_tokens\":8}}}}"); }
    if model == "nonzero" { std::process::exit(4); }
}
