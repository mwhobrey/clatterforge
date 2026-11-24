use clatterforge::{inference::state::Coordinator, process::current_identity};
use tempfile::TempDir;

#[test]
fn occupied_resource_error_names_the_resource_and_inspection_command() {
    let temp = TempDir::new().unwrap();
    let mut c = Coordinator::open(&temp.path().join("admission.sqlite")).unwrap();
    let owner = current_identity().unwrap();
    c.acquire("shared-fixture-gpu0", "holder", &owner).unwrap();
    let before = c.reservations().unwrap();
    let message = c
        .acquire("shared-fixture-gpu0", "waiting", &owner)
        .unwrap_err()
        .to_string();
    assert!(message.contains("shared-fixture-gpu0"), "{message}");
    assert!(message.contains("inference-resources"), "{message}");
    assert!(message.contains("no dispatch"), "{message}");
    assert_eq!(c.reservations().unwrap(), before);
}

#[test]
fn quarantine_remains_reserved_after_the_clearer_failure() {
    let temp = TempDir::new().unwrap();
    let mut c = Coordinator::open(&temp.path().join("admission.sqlite")).unwrap();
    let owner = current_identity().unwrap();
    c.acquire("shared-fixture-gpu0", "interrupted", &owner)
        .unwrap();
    c.quarantine_job("interrupted").unwrap();
    c.release("shared-fixture-gpu0", "interrupted").unwrap();
    let before = c.reservations().unwrap();
    let message = c
        .acquire("shared-fixture-gpu0", "new", &owner)
        .unwrap_err()
        .to_string();
    assert!(message.contains("shared-fixture-gpu0"), "{message}");
    assert!(message.contains("quarantined"), "{message}");
    assert!(message.contains("inference-resources"), "{message}");
    assert!(message.contains("no dispatch"), "{message}");
    assert_eq!(c.reservations().unwrap(), before);
    c.acquire("independent-resource", "peer", &owner).unwrap();
    c.release("independent-resource", "peer").unwrap();
    assert_eq!(c.reservations().unwrap(), before);
}
