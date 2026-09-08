//! Does the pg_duckdb archive actually load, on each PostgreSQL major?
//!
//! An extension archive that installs cleanly and fails at CREATE EXTENSION is
//! a miserable thing to debug, and the module carries the major version it was
//! compiled against, so each one has to be tried against its own server.
use dbcenter_core::DBCenter;

fn main() {
    let majors: Vec<String> = std::env::args().skip(1).collect();
    let majors = if majors.is_empty() {
        vec!["18".to_string()]
    } else {
        majors
    };

    for major in majors {
        let path = std::env::temp_dir().join(format!("dbcenter_duck_pg{major}"));
        let _ = std::fs::remove_dir_all(&path);
        print!("PostgreSQL {major}: ");

        let db = match DBCenter::builder()
            .path(&path)
            .persist(false)
            .postgres_version(&major)
            .build()
        {
            Ok(db) => db,
            Err(e) => {
                println!("server would not start: {e}");
                continue;
            }
        };

        match db.install_extension("pg_duckdb") {
            Ok(()) => {
                let rows = db
                    .query(
                        "SELECT r['v']::bigint AS v FROM duckdb.query($$ SELECT 42 AS v $$) r",
                        &[],
                    )
                    .unwrap();
                let answer = rows[0]["v"].as_i64().unwrap();
                assert_eq!(answer, 42);
                let version = db.query("SELECT version() AS v", &[]).unwrap()[0]["v"]
                    .as_str()
                    .unwrap()
                    .split(' ')
                    .nth(1)
                    .unwrap()
                    .to_string();
                println!("ok, duckdb answers through PostgreSQL {version}");
            }
            Err(e) => println!("install failed: {e}"),
        }
        db.shutdown().ok();
        let _ = std::fs::remove_dir_all(&path);
    }
}
