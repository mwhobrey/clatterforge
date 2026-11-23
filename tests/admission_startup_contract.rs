use dev_conductor::{
    inference::state::{Coordinator, JobStore},
    process::current_identity,
};
use rusqlite::Connection;
use std::{
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn simultaneous_first_open_converges_without_losing_reservations() {
    let temp = TempDir::new().unwrap();
    let owner = current_identity().unwrap();
    for round in 0..16 {
        let path = temp.path().join(format!("shared-{round}.sqlite"));
        let barrier = Arc::new(Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                let barrier = barrier.clone();
                let owner = owner.clone();
                thread::spawn(move || -> anyhow::Result<()> {
                    barrier.wait();
                    let mut c = Coordinator::open(&path)?;
                    c.acquire(&format!("resource-{i}"), &format!("job-{i}"), &owner)?;
                    c.quarantine_job(&format!("job-{i}"))?;
                    Ok(())
                })
            })
            .collect();
        let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert!(
            outcomes.iter().all(Result::is_ok),
            "round {round}: {outcomes:?}"
        );
        let c = Coordinator::open(&path).unwrap();
        let reservations = c.reservations().unwrap();
        assert_eq!(reservations.len(), 8);
        assert!(reservations.iter().all(|r| r["quarantined"] == true));
    }
}

#[test]
fn simultaneous_job_store_initialization_also_converges() {
    let temp = TempDir::new().unwrap();
    for round in 0..8 {
        let root = temp.path().join(format!("jobs-{round}"));
        let barrier = Arc::new(Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    JobStore::open(&root).map(|_| ())
                })
            })
            .collect();
        let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert!(
            outcomes.iter().all(Result::is_ok),
            "round {round}: {outcomes:?}"
        );
    }
}

#[test]
fn newer_and_invalid_databases_fail_without_reset_or_busy_retry() {
    let temp = TempDir::new().unwrap();
    let future = temp.path().join("future.sqlite");
    let db = Connection::open(&future).unwrap();
    db.execute_batch("PRAGMA user_version=2; CREATE TABLE sentinel(value TEXT); INSERT INTO sentinel VALUES('keep');").unwrap();
    let start = Instant::now();
    let error = Coordinator::open(&future).err().unwrap();
    assert!(
        error
            .to_string()
            .contains("newer inference/coordinator schema")
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT value FROM sentinel", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "keep"
    );
    let bad = temp.path().join("invalid.sqlite");
    std::fs::write(&bad, b"this is not a sqlite database").unwrap();
    let before = std::fs::read(&bad).unwrap();
    let start = Instant::now();
    assert!(Coordinator::open(&bad).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(std::fs::read(&bad).unwrap(), before);
}

#[test]
fn persistent_startup_lock_returns_an_error_within_a_bounded_wait() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("locked.sqlite");
    let lock = Connection::open(&path).unwrap();
    lock.execute_batch("CREATE TABLE sentinel(value TEXT); BEGIN EXCLUSIVE;")
        .unwrap();
    let start = Instant::now();
    assert!(Coordinator::open(&path).is_err());
    assert!(
        start.elapsed() < Duration::from_secs(7),
        "initialization exceeded its bounded wait"
    );
    lock.execute_batch("ROLLBACK;").unwrap();
    assert!(Coordinator::open(&path).is_ok());
}
