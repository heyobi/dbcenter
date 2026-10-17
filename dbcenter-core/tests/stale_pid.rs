//! A server that was killed leaves its postmaster.pid and socket lock behind.
//!
//! Linux only: the check reads /proc, and the stand-in process is a `sleep`.
#![cfg(target_os = "linux")]

use dbcenter_core::DBCenter;

#[test]
fn a_stale_pid_file_naming_a_live_process_that_is_not_postgres_is_ignored() {
    // In a container the pid a killed server left behind is soon some other
    // process of the next run, so "is that pid alive" said yes and the node
    // refused to start for good. A `sleep` stands in for that process: alive,
    // the same user's, and not a postgres. Not this test process: PostgreSQL
    // knows a lock naming its own parent is stale, and would prove nothing.
    let path = std::env::temp_dir().join("dbcenter_stale_pid");
    let _ = std::fs::remove_dir_all(&path);

    let db = DBCenter::builder()
        .path(&path)
        .persist(true)
        .build()
        .expect("first start");
    db.shutdown().unwrap();
    drop(db);

    let mut other = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    std::fs::write(
        path.join("postmaster.pid"),
        format!("{}\n{}\n", other.id(), path.display()),
    )
    .unwrap();
    // PostgreSQL checks the socket's own lock as well, and is taken in by a
    // reused pid in the same way.
    std::fs::write(
        path.join("sockets").join(".s.PGSQL.5432.lock"),
        format!("{}\n{}\n", other.id(), path.display()),
    )
    .unwrap();

    let db = DBCenter::builder()
        .path(&path)
        .persist(true)
        .build()
        .expect("a stale postmaster.pid must not stop the server starting");
    db.shutdown().unwrap();
    let _ = other.kill();
    let _ = other.wait();
    drop(db);
    let _ = std::fs::remove_dir_all(&path);
}
