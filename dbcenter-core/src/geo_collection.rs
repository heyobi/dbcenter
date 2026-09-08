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
            tracing::error!(
                "Failed to initialize GeoCollection table/index '{}': {}",
                name,
                e
            );
        }

        collection
    }

    fn ensure_table_and_index(&self) -> Result<()> {
        let sql = format!(
            r#"
            CREATE TABLE IF NOT EXISTS "{}" (
                id SERIAL PRIMARY KEY,
                data JSONB,
                "lat" DOUBLE PRECISION,
                "lon" DOUBLE PRECISION,
                "geometry" JSONB,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            
            -- Standard GIN index for JSON data
            CREATE INDEX IF NOT EXISTS idx_{}_data ON "{}" USING GIN (data);

            -- Spatial index on (lon, lat)
            -- We cast to float. Note: using 4326 (WGS 84)
            CREATE INDEX IF NOT EXISTS idx_{}_geo ON "{}" USING GIST (
                ST_SetSRID(ST_MakePoint("lon", "lat"), 4326)
            );
            "#,
            self.name, self.name, self.name, self.name, self.name
        );

        let mut client = self.db.get_client();
        client.batch_execute(&sql).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "GeoCollection init failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        Ok(())
    }

    /// Does this table have the given column?
    fn has_column(&self, column: &str) -> Result<bool> {
        let mut client = self.db.get_client();
        let row = client
            .query_one(
                "SELECT EXISTS (
                     SELECT 1 FROM information_schema.columns
                     WHERE table_name = $1 AND column_name = $2
                 )",
                &[&self.name, &column],
            )
            .map_err(|e| {
                crate::error::DBCenterError::QueryError(format!(
                    "Column lookup failed: {}",
                    crate::utils::format_postgres_error(&e)
                ))
            })?;
        Ok(row.get(0))
    }

    /// A WHERE condition selecting the row an id refers to.
    ///
    /// A number is the `id` primary key. A string is matched against whatever the
    /// table actually has: `name` in the relational layout, `data->>'name'` and
    /// `data->>'id'` in the JSONB one.
    ///
    /// This used to look only in `data`. Geo inserts go through the relational
    /// path, which writes each field to its own column and leaves `data` null, so
    /// every string lookup matched nothing - and both callers report that as "not
    /// found" rather than an error, so `distance_between` quietly returned None
    /// and `contains_point` quietly returned false.
    fn row_condition(&self, id: &JsonValue) -> Result<String> {
        if let JsonValue::Number(n) = id {
            return Ok(format!("id = {}", n));
        }

        let value = match id {
            JsonValue::String(s) => s.clone(),
            other => other.to_string(),
        };
        let literal =
            crate::query::QueryBuilder::json_to_sql_literal(&JsonValue::String(value.clone()));

        let mut alternatives = Vec::new();
        if self.has_column("name")? {
            alternatives.push(format!("\"name\" = {}", literal));
        }
        if self.has_column("data")? {
            alternatives.push(format!("data->>'name' = {}", literal));
            alternatives.push(format!("data->>'id' = {}", literal));
        }

        if alternatives.is_empty() {
            return Err(crate::error::DBCenterError::InvalidQuery(format!(
                "'{}' cannot be resolved: collection '{}' has neither a 'name' \
                 column nor a 'data' column to match it against. Pass the numeric id.",
                value, self.name
            )));
        }
        Ok(format!("({})", alternatives.join(" OR ")))
    }

    /// Insert documents (delegates to standard collection)
    pub fn insert_many(&self, documents: &[JsonValue]) -> Result<Vec<String>> {
        self.db.collection(&self.name).insert_many(documents)
    }

    /// Find points within radius (km)
    pub fn find_within_radius(&self, lat: f64, lon: f64, radius_km: f64) -> Result<Vec<JsonValue>> {
        // PostGIS ST_DWithin uses meters for geography type
        let radius_meters = radius_km * 1000.0;

        // Query using relational columns "lon", "lat"
        let sql = format!(
            r#"
            SELECT id, to_jsonb(t) as doc
            FROM "{}" t
            WHERE ST_DWithin(
                ST_SetSRID(ST_MakePoint("lon", "lat"), 4326)::geography,
                ST_SetSRID(ST_MakePoint($1, $2), 4326)::geography,
                $3
            )
            "#,
            self.name
        );

        let mut client = self.db.get_client();
        let rows = client
            .query(&sql, &[&lon, &lat, &radius_meters])
            .map_err(|e| {
                crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
            })?;

        let mut results = Vec::new();
        for row in rows {
            // let id: i32 = row.get(0);
            let mut data: JsonValue = row.get(1);

            // Ensure ID is present in the document
            if let JsonValue::Object(ref mut map) = data {
                // Map id to _id if present in fetched row (it's in to_jsonb(t))
                if let Some(id_val) = map.get("id") {
                    map.insert("_id".to_string(), id_val.clone());
                }
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
    /// Great-circle distance in metres between two documents' points.
    ///
    /// Each argument identifies one document. A number matches the `id` primary
    /// key; a string matches either `data->>'id'` or `data->>'name'`, so callers
    /// can refer to a place by its name without knowing the generated id.
    /// Returns `None` when either document is missing or has no coordinates.
    pub fn distance_between(&self, from_id: &JsonValue, to_id: &JsonValue) -> Result<Option<f64>> {
        let cond1 = self.row_condition(from_id)?;
        let cond2 = self.row_condition(to_id)?;

        let sql = format!(
            r#"
            WITH p1 AS (SELECT "lon", "lat" FROM "{0}" WHERE {1} LIMIT 1),
                 p2 AS (SELECT "lon", "lat" FROM "{0}" WHERE {2} LIMIT 1)
            SELECT 
                ST_Distance(
                    ST_SetSRID(ST_MakePoint(p1.lon, p1.lat), 4326)::geography,
                    ST_SetSRID(ST_MakePoint(p2.lon, p2.lat), 4326)::geography
                )
            FROM p1, p2
            "#,
            self.name, cond1, cond2
        );

        let mut client = self.db.get_client();
        let rows = client.query(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        if let Some(row) = rows.first() {
            let dist: Option<f64> = row.get(0);
            return Ok(dist);
        }

        Ok(None)
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
        let cond = self.row_condition(polygon_id)?;

        // ST_Contains(polygon, point)
        // Use "geometry" column
        let sql = format!(
            r#"
            SELECT 
                ST_Contains(
                    ST_SetSRID(ST_GeomFromGeoJSON("geometry"), 4326),
                    ST_SetSRID(ST_MakePoint($1, $2), 4326)
                )
            FROM "{}"
            WHERE {}
            LIMIT 1
            "#,
            self.name, cond
        );

        let mut client = self.db.get_client();
        let rows = client.query(&sql, &[&lon, &lat]).map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        if let Some(row) = rows.first() {
            let contains: Option<bool> = row.get(0);
            return Ok(contains.unwrap_or(false));
        }

        Ok(false)
    }
}
