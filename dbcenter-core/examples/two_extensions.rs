//! Two bundled extensions in one PostgreSQL, both still working.
//!
//! Every bundle used to unpack its libraries straight into PostgreSQL's module
//! directory, which all extensions share. PostGIS and pg_duckdb bundle
//! twenty-six of the same sonames, built against different base images, so
//! whichever was installed second overwrote the other's copies - and the first
//! then had a library it was never linked against. Installing both broke one of
//! them, with an undefined symbol at server startup.
use dbcenter_core::DBCenter;

fn main() {
    for order in [["postgis", "pg_duckdb"], ["pg_duckdb", "postgis"]] {
        let path = std::env::temp_dir().join(format!("dbcenter_two_{}", order[0]));
        let _ = std::fs::remove_dir_all(&path);
        println!("\ninstalling {} then {}", order[0], order[1]);

        let db = DBCenter::builder()
            .path(&path)
            .persist(false)
            .build()
            .unwrap();
        for name in order {
            db.install_extension(name)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            println!("  {name} installed");
        }

        // Both have to still answer, not just still be listed.
        let km: f64 = db
            .query(
                "SELECT ST_Distance(ST_MakePoint(28.98,41.01)::geography, \
                 ST_MakePoint(32.85,39.93)::geography) / 1000 AS km",
                &[],
            )
            .unwrap()[0]["km"]
            .as_f64()
            .unwrap();
        let answer = db
            .query(
                "SELECT r['v']::bigint AS v FROM duckdb.query($$ SELECT 42 AS v $$) r",
                &[],
            )
            .unwrap()[0]["v"]
            .as_i64()
            .unwrap();

        // Istanbul to Ankara on the spheroid. The point is that PostGIS is
        // computing a geodesic at all - a broken GEOS or PROJ shows up as a
        // failure to load, not a wrong number - so the band is generous.
        assert!((340.0..360.0).contains(&km), "postgis gave {km} km");
        assert_eq!(answer, 42);
        println!("  postgis: {km:.1} km, duckdb: {answer}");

        db.shutdown().unwrap();
        let _ = std::fs::remove_dir_all(&path);
    }
    println!("\nboth orders work");
}
