//! A server that was killed leaves its postmaster.pid behind.

use dbcenter_core::DBCenter;

#[test]
fn a_stale_pid_file_naming_a_live_process_that_is_not_postgres_is_ignored() {
    // In a container the pid a killed server left behind is soon some other
    // process of the next run, so "is that pid alive" said yes and the node
    // refused to start for good. This test process stands in for that
    // process: alive, and not a postgres.
    let path = std::env::temp_dir().join("dbcenter_stale_pid");
    let _ = std::fs::remove_dir_all(&path);

    let db = DBCenter::builder()
        .path(&path)
        .persist(true)
        .build()
        .expect("first start");
    db.shutdown().unwrap();
    drop(db);

    std::fs::write(
        path.join("postmaster.pid"),
        format!("{}\n{}\n", std::process::id(), path.display()),
    )
    .unwrap();

    let db = DBCenter::builder()
        .path(&path)
        .persist(true)
        .build()
        .expect("a stale postmaster.pid must not stop the server starting");
    db.shutdown().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&path);
}
