// M10.3 Phase 5: the independent, pinned check for the live Codex CLI
// acceptance test's exact-content file-creation assignment. Deliberately
// standalone (compiled via rustc, exactly like this crate's other test
// fixtures), not part of the main crate - it must not be influenced by
// anything the assignment or the agent could touch.
//
// argv[1] is the expected exact content (passed as a pinned
// `AgentSpec`-style `--verify-arg`, analogous to `orchestration_verifier.rs`
// reading its own expected value from `verification.txt`). Checks the new
// file exists with exactly that content and that the pre-existing source
// file the assignment must not touch is still present and unchanged.
use std::fs;

fn main() {
    let expected = std::env::args()
        .nth(1)
        .expect("expected content argument required");
    let actual = match fs::read_to_string("codex_result.txt") {
        Ok(text) => text,
        Err(error) => {
            eprintln!("codex_result.txt missing or unreadable: {error}");
            std::process::exit(7);
        }
    };
    if actual != expected {
        eprintln!("codex_result.txt content mismatch: got {actual:?}, want {expected:?}");
        std::process::exit(8);
    }
    let protected = match fs::read_to_string("src/lib.rs") {
        Ok(text) => text,
        Err(error) => {
            eprintln!("protected src/lib.rs missing: {error}");
            std::process::exit(9);
        }
    };
    if !protected.contains("unused by the Codex assignment") {
        eprintln!("protected src/lib.rs content changed unexpectedly");
        std::process::exit(10);
    }
    println!("codex_result.txt matches the exact expected content; protected source untouched");
}
