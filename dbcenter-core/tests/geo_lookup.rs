//! Geo lookups by name.
//!
//! Geo inserts go through the relational path, which writes each field to its own
//! column and leaves the table's `data` JSONB column null. The lookups matched
//! against `data->>'name'`, so every one of them found nothing - and both callers
//! report nothing found as None or false rather than an error, so the failure was
//! silent.

use dbcenter_core::DBCenter;
use serde_json::json;

#[test]
fn a_place_can_be_found_by_name() {
    let path = std::env::temp_dir().join("dbcenter_geo_lookup");
    let _ = std::fs::remove_dir_all(&path);
    let db = DBCenter::builder()
        .path(&path)
        .persist(false)
        .build()
        .expect("embedded postgres should start");

    if let Err(e) = db.install_extension("postgis") {
        eprintln!("skipping: PostGIS is not available here ({e})");
        return;
    }

    let geo = db.geo_collection("stores");
    geo.insert_many(&[
        json!({"name": "Istanbul", "lat": 41.0, "lon": 29.0}),
        json!({"name": "Ankara", "lat": 39.93, "lon": 32.85}),
        json!({
            "name": "Marmara",
            "lat": 40.7, "lon": 28.0,
            "geometry": {
                "type": "Polygon",
                "coordinates": [[[27.0, 40.0], [30.0, 40.0], [30.0, 41.5], [27.0, 41.5], [27.0, 40.0]]]
            }
        }),
    ])
    .unwrap();

    // Roughly 350km apart; the tolerance is for the ellipsoid model, not for doubt.
    let metres = geo
        .distance_between(&json!("Istanbul"), &json!("Ankara"))
        .unwrap()
        .expect("both places exist, so there is a distance between them");
    assert!(
        (340_000.0..360_000.0).contains(&metres),
        "Istanbul to Ankara came out as {metres} metres"
    );

    // The numeric primary key has to keep working too.
    assert!(geo
        .distance_between(&json!(1), &json!(2))
        .unwrap()
        .is_some());

    // A place that is not there is None, which is different from a lookup that
    // cannot work - that distinction was exactly what was lost.
    assert!(geo
        .distance_between(&json!("Nowhere"), &json!("Ankara"))
        .unwrap()
        .is_none());

    // Inside the polygon, and outside it.
    assert!(geo.contains_point(&json!("Marmara"), 40.7, 28.5).unwrap());
    assert!(!geo.contains_point(&json!("Marmara"), 10.0, 10.0).unwrap());

    // Radius search reads the lat/lon columns and always worked.
    let near = geo.find_within_radius(41.0, 29.0, 50.0).unwrap();
    assert_eq!(near.len(), 1);
    assert_eq!(near[0]["name"], json!("Istanbul"));

    db.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(&path);
}
