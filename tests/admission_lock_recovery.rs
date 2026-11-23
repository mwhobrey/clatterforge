use dev_conductor::{inference::state::Coordinator, process::current_identity};
use rusqlite::Connection;
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn startup_recovers_after_unlock_and_normal_writes_still_wait() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("shared.sqlite");
    let lock = Connection::open(&path).unwrap();
    lock.execute_batch("CREATE TABLE sentinel(value TEXT); BEGIN EXCLUSIVE;")
        .unwrap();
    let copy = path.clone();
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        tx.send(()).unwrap();
        let start = Instant::now();
        let c = Coordinator::open(&copy).unwrap();
        (c, start.elapsed())
    });
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    thread::sleep(Duration::from_millis(200));
    lock.execute_batch("ROLLBACK;").unwrap();
    let (mut coordinator, elapsed) = worker.join().unwrap();
    assert!(elapsed >= Duration::from_millis(150));
    assert!(elapsed < Duration::from_secs(2));

    lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let (tx, rx) = mpsc::channel();
    let owner = current_identity().unwrap();
    let worker = thread::spawn(move || {
        tx.send(()).unwrap();
        coordinator.acquire("gpu", "job", &owner).unwrap();
        coordinator.quarantine_job("job").unwrap();
        assert!(coordinator.acquire("gpu", "intruder", &owner).is_err());
        coordinator
    });
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    thread::sleep(Duration::from_millis(200));
    lock.execute_batch("ROLLBACK;").unwrap();
    let coordinator = worker.join().unwrap();
    let reservations = coordinator.reservations().unwrap();
    assert_eq!(reservations.len(), 1);
    assert_eq!(reservations[0]["job"], "job");
    assert_eq!(reservations[0]["quarantined"], true);
}
