use crate::error::Result;
use crate::DBCenter;
use serde_json::Value as JsonValue;

/// GeoCollection wrapper - Geospatial API for PostgreSQL
pub struct GeoCollection<'a> {
    db: &'a DBCenter,
    name: String,
}

impl<'a> GeoCollection<'a> {
    pub fn new(db: &'a DBCenter, name: &str) -> Self {
        let collection = Self {
            db,
            name: name.to_string(),
        };

        // Ensure PostGIS extension is installed
        if let Err(e) = db.install_extension("postgis") {
             tracing::error!("Failed to install PostGIS for GeoCollection: {}", e);
        }

        // Ensure table exists (delegate to standard collection logic via a temporary instance)
        // or just create it directly. Since standard collection handles it well, let's just use specific logic here
        // to ensure we have control, but re-using would be cleaner. 
        // We can just ensure the table exists by "instantiating" a standard collection temporarily
        // or just running the CREATE TABLE SQL if we want to be explicit.
        // For simplicity and robustness, let's explicitly create it here or assume the user might have used it as a normal collection before.
        // Actually, we should just ensure it exists.
        if let Err(e) = collection.ensure_table_and_index() {
            tracing::error!("Failed to initialize GeoCollection table/index '{}': {}", name, e);
        }

        collection
    }

    fn ensure_table_and_index(&self) -> Result<()> {
        let sql = format!(
            r#"
            CREATE TABLE IF NOT EXISTS "{}" (
                id SERIAL PRIMARY KEY,
                data JSONB,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            
            -- Standard GIN index for JSON data
            CREATE INDEX IF NOT EXISTS idx_{}_data ON "{}" USING GIN (data);

            -- Spatial index on (lon, lat)
            -- We cast to float. Note: using 4326 (WGS 84)
            CREATE INDEX IF NOT EXISTS idx_{}_geo ON "{}" USING GIST (
                ST_SetSRID(ST_MakePoint(
                    CAST(data->>'lon' AS FLOAT), 
                    CAST(data->>'lat' AS FLOAT)
                ), 4326)
            );
            "#,
            self.name, 
            self.name, self.name,
            self.name, self.name
        );

        let mut client = self.db.get_client();
        client.batch_execute(&sql).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!("GeoCollection init failed: {}", crate::utils::format_postgres_error(&e)))
        })?;

        Ok(())
    }

    /// Insert documents (delegates to standard collection)
    pub fn insert_many(&self, documents: &[JsonValue]) -> Result<Vec<String>> {
        self.db.collection(&self.name).insert_many(documents)
    }

    /// Find points within radius (km)
    pub fn find_within_radius(&self, lat: f64, lon: f64, radius_km: f64) -> Result<Vec<JsonValue>> {
        // PostGIS ST_DWithin uses meters for geography type
        let radius_meters = radius_km * 1000.0;
        
        // Query using the index we created
        // We use geography type for accurate distance calculation in meters
        let sql = format!(
            r#"
            SELECT id, data 
            FROM "{}"
            WHERE ST_DWithin(
                ST_SetSRID(ST_MakePoint(CAST(data->>'lon' AS FLOAT), CAST(data->>'lat' AS FLOAT)), 4326)::geography,
                ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography,
                $3
            )
            "#,
            self.name
        );

        let mut client = self.db.get_client();
        let rows = client.query(&sql, &[&lon, &lat, &radius_meters])
            .map_err(|e| crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e)))?;

        let mut results = Vec::new();
        for row in rows {
            let id: i32 = row.get(0);
            let mut data: JsonValue = row.get(1);
            
            if let JsonValue::Object(ref mut map) = data {
                map.insert("_id".to_string(), JsonValue::Number(id.into()));
            }
            results.push(data);
        }

        Ok(results)
    }

    /// Calculate distance between two documents (identified by their IDs or some field - user request says `from_id`, `to_id`. 
    /// The user example uses "store_a" which implies string IDs inside the doc or the auto-increment ID?
    /// User code: `stores.insert_many([...])` -> these get auto IDs. 
    /// User query: `distance_between(from_id="store_a", to_id="store_b")`.
    /// Wait, the inserted docs have `name: Store A`. It's likely the user intends to query by some field.
    /// BUT `from_id` implies the primary key or a specific unique field.
    /// The DB auto-generates integer IDs. 
    /// However, usually in such APIs, if you pass a string, it might be looking up by `_id` (if string) or maybe a custom ID field?
    /// The user prompt `from_id="store_a"` is tricky because the inserted data doesn't have `id: "store_a"`.
    /// It has `name: "Store A"`.
    /// Maybe the user IMPLIED they manually set IDs or the example assumes they retrieved IDs?
    /// OR, maybe I should support looking up by `data->>'id'` or `data->>'name'`?
    /// Let's assume for now that if it's a string, we look in `data->>'id'` OR `data->>'name'` OR `id` if it parses to int?
    /// To be safe and flexible, let's implement `distance_between_points(lat1, lon1, lat2, lon2)` and `distance_between_ids(id1, id2)`.
    /// But to match the user request exactly: `stores.distance_between(from_id="store_a", ...)`
    /// I'll implement it to take `JsonValue` as IDs. If string, check `data->>'id'` or `data->>'name'`. If int, check PK.
    /// Actually, to simplify and be robust:
    /// Let's implement `distance_between` taking two generic criteria (JsonValue).
    /// But typically `id` means the PK. The user's example is slightly inconsistent with the insert (no explicit ID).
    /// I will assume the user considers `name` or some unique identifier.
    /// Let's query the two documents first, then calculate.
    /// Or do it in one SQL.
    /// Let's assume `id` refers to the `id` column (integer) OR a field named `id` in JSON. 
    /// Given the user example: `from_id="store_a"`, and the insert had `name="Store A"`.
    /// Maybe they meant `name`?
    /// I will interpret `from_id` as: look for document where `id` (PK) = val OR `data->>'id'` = val OR `data->>'name'` = val.
    
    pub fn distance_between(&self, from_id: &JsonValue, to_id: &JsonValue) -> Result<Option<f64>> {
        // Helper to generate SQL condition for an ID
        let id_condition = |param_idx: usize| {
            format!(
                "(id = ${0}::text::int OR data->>'id' = ${0} OR data->>'name' = ${0})", 
                param_idx
            )
        };
        // We use text for parameter to allow both string and int matching (via cast for PK)
        // But if param is stricly int, `id = $1` works. 
        // Postgres casting from text to int might fail if text is not int.
        
        // Safer approach: Query by keys.
        // Let's iterate: find doc1, find doc2. Then calc distance.
        // Or single query.
        
        // Let's assume strictly: 
        // If from_id is Number -> PK.
        // If String -> `data->>'id'` or `data->>'name'`.
        
        let (val1, is_int1) = self.extract_id_val(from_id);
        let (val2, is_int2) = self.extract_id_val(to_id);

        let get_cond = |val: &String, is_int: bool, placeholder: &str| {
            if is_int {
                format!("id = {}", val) // unsafe if val not sanitized? val comes from number, safe.
            } else {
                format!("(data->>'id' = '{}' OR data->>'name' = '{}')", val.replace("'", "''"), val.replace("'", "''"))
            }
        };

        let cond1 = get_cond(&val1, is_int1, "");
        let cond2 = get_cond(&val2, is_int2, "");

        let sql = format!(
            r#"
            WITH p1 AS (SELECT data FROM "{0}" WHERE {1} LIMIT 1),
                 p2 AS (SELECT data FROM "{0}" WHERE {2} LIMIT 1)
            SELECT 
                ST_Distance(
                    ST_SetSRID(ST_MakePoint((p1.data->>'lon')::float, (p1.data->>'lat')::float), 4326)::geography,
                    ST_SetSRID(ST_MakePoint((p2.data->>'lon')::float, (p2.data->>'lat')::float), 4326)::geography
                )
            FROM p1, p2
            "#,
            self.name, cond1, cond2
        );

        let mut client = self.db.get_client();
        let rows = client.query(&sql, &[])
            .map_err(|e| crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e)))?;

        if let Some(row) = rows.first() {
            let dist: Option<f64> = row.get(0);
            return Ok(dist);
        }

        Ok(None)
    }

    fn extract_id_val(&self, v: &JsonValue) -> (String, bool) {
        match v {
            JsonValue::Number(n) => (n.to_string(), true),
            JsonValue::String(s) => (s.clone(), false),
            _ => (v.to_string(), false),
        }
    }
    
    /// Check if point is inside a polygon (identified by ID)
    /// Polygon is assumed to be in `data->'geometry'` or `data->'polygon'`?
    /// User prompt says: `contains_point(polygon_id="...", lat=..., lon=...)`.
    /// Example usage `is_inside = stores.contains_point(...)`.
    /// This implies the polygon is stored in the SAME collection as documents?
    /// Or maybe "delivery_zone_1" is just another document in "stores" collection that has a polygon shape instead of point?
    /// If so, the collection is mixed-type (points and polygons).
    /// PostGIS functions like ST_Contains(geomA, point) work if geomA is polygon.
    /// We need to extract the geometry from the document.
    /// Let's assume the doc can have `lat`/`lon` OR `geojson` / `geometry` field.
    /// The user didn't specify format. I will try to handle `geometry` (GeoJSON) or `wkt`.
    /// Most likely GeoJSON in `data`.
    pub fn contains_point(&self, polygon_id: &JsonValue, lat: f64, lon: f64) -> Result<bool> {
         let (val, is_int) = self.extract_id_val(polygon_id);
         let get_cond = |val: &String, is_int: bool| {
            if is_int {
                format!("id = {}", val)
            } else {
                format!("(data->>'id' = '{}' OR data->>'name' = '{}')", val.replace("'", "''"), val.replace("'", "''"))
            }
        };
        let cond = get_cond(&val, is_int);

        // ST_Contains(polygon, point)
        // We assume the stored document has a field that can be cast to geometry.
        // Try `data->>'geometry'` (GeoJSON string or WKT) or `data->'geometry'` (GeoJSON object).
        // ST_GeomFromGeoJSON(data->'geometry') is standard.
        
        let sql = format!(
            r#"
            SELECT 
                ST_Contains(
                    ST_SetSRID(ST_GeomFromGeoJSON(data->'geometry'), 4326),
                    ST_SetSRID(ST_MakePoint($1, $2), 4326)
                )
            FROM "{}"
            WHERE {}
            LIMIT 1
            "#,
            self.name, cond
        );

        let mut client = self.db.get_client();
        let rows = client.query(&sql, &[&lon, &lat])
            .map_err(|e| crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e)))?;

         if let Some(row) = rows.first() {
            let contains: Option<bool> = row.get(0);
            return Ok(contains.unwrap_or(false));
         }

        Ok(false)
    }

}
