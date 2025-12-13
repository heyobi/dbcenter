use crate::DBCenter;
use crate::error::{DBCenterError, Result};
use crate::utils; // Assuming utils is accessible and has format_postgres_error

pub struct DeltaManager;

impl DeltaManager {
    pub fn ensure_delta_tables(db: &DBCenter, table_name: &str) -> Result<()> {
        let base = format!("{}_base", table_name);
        let adds = format!("{}_adds", table_name);
        let dels = format!("{}_dels", table_name);

        let mut client = db.get_client();

        // 1. Create Physical Tables
        
        // x_base: The read-only heavy data table
        // We use the standard schema
        let sql_base = format!(
            r#"
            CREATE TABLE IF NOT EXISTS "{}" (
                id INTEGER PRIMARY KEY, -- Not SERIAL to avoid conflict, managed externally or via adds logic? actually let's keep it INTEGER for base
                data JSONB,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_{}_data ON "{}" USING GIN (data);
            "#, 
            base, base, base
        );

        // x_adds: The new inserts/updates
        // Use SERIAL here to generate new IDs for inserts
        let sql_adds = format!(
            r#"
            CREATE TABLE IF NOT EXISTS "{}" (
                id SERIAL PRIMARY KEY,
                data JSONB,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_{}_data ON "{}" USING GIN (data);
            "#, 
            adds, adds, adds
        );

        // x_dels: Tombstones
        let sql_dels = format!(
            r#"
            CREATE TABLE IF NOT EXISTS "{}" (
                id INTEGER PRIMARY KEY,
                deleted_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            "#, 
            dels
        );

        client.batch_execute(&format!("{}{}{}", sql_base, sql_adds, sql_dels)).map_err(|e| {
             DBCenterError::QueryError(format!("Failed to create delta physical tables: {}", e))
        })?;

        // 2. Create Create "Transparent View"
        // Union of base and adds, filtering out dels
        // Logic: 
        // Rows from ADDS (new or updated)
        // UNION ALL
        // Rows from BASE (excluding those updated in ADDS or deleted in DELS)
        
        let view_sql = format!(
            r#"
            CREATE OR REPLACE VIEW "{}" AS
            SELECT * FROM "{}"
            UNION ALL
            SELECT * FROM "{}" WHERE id NOT IN (SELECT id FROM "{}") AND id NOT IN (SELECT id FROM "{}");
            "#,
            table_name,
            adds, // Adds - NO DELS FILTER
            base, adds, dels // Base - Filtered by both
        );

        client.batch_execute(&view_sql).map_err(|e| {
             DBCenterError::QueryError(format!("Failed to create delta view: {}", e))
        })?;

        // 3. Implement INSTEAD OF TRIGGERS
        
        // INSERT trigger
        let insert_trigger_func = format!("func_insert_{}", table_name);
        let insert_sql = format!(
            r#"
            CREATE OR REPLACE FUNCTION {}() RETURNS TRIGGER AS $$
            BEGIN
                INSERT INTO "{}" (data) VALUES (NEW.data) RETURNING id, created_at, updated_at INTO NEW.id, NEW.created_at, NEW.updated_at;
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;

            DROP TRIGGER IF EXISTS trg_insert_{} ON "{}";
            CREATE TRIGGER trg_insert_{}
            INSTEAD OF INSERT ON "{}"
            FOR EACH ROW EXECUTE FUNCTION {}();
            "#,
            insert_trigger_func, adds,
            table_name, table_name,
            table_name, table_name,
            insert_trigger_func
        );

        // UPDATE trigger
        // Mark old ID as deleted (in dels)? 
        // If we update, we insert new version into ADDs.
        // But what about the ID?
        // If we want to preserve ID (standard SQL update), we must insert into ADDS with the SAME ID.
        // But ADDS has `id SERIAL PRIMARY KEY`. If we force ID, it works.
        // So:
        // 1. Add OLD.id to x_dels (to hide the version in Base or previous Adds).
        // 2. Insert (OLD.id, NEW.data) into x_adds.
        // ON CONFLICT on x_adds? If we updated it twice?
        // If ID is PK in x_adds, we must UPDATE x_adds if it exists there, or INSERT if not.
        // Or simply: DELETE FROM x_adds WHERE id = OLD.id; INSERT INTO x_adds ...
        // Wait, if it's in Base, it's not in Adds.
        // If it's in Adds, it's in Adds.
        // So:
        // 1. DELETE FROM x_adds WHERE id = OLD.id; (Clean up potential previous add)
        // 2. INSERT INTO x_dels (id) VALUES (OLD.id) ON CONFLICT DO NOTHING; (Hide base)
        // 3. INSERT INTO x_adds (id, data) VALUES (OLD.id, NEW.data); (Add new version)
        
        let update_trigger_func = format!("func_update_{}", table_name);
        let update_sql = format!(
            r#"
            CREATE OR REPLACE FUNCTION {}() RETURNS TRIGGER AS $$
            BEGIN
                -- Remove from adds if exists (we are replacing it)
                DELETE FROM "{}" WHERE id = OLD.id;
                
                -- Mark as deleted (hides base version)
                INSERT INTO "{}" (id) VALUES (OLD.id) ON CONFLICT (id) DO NOTHING;
                
                -- Insert new version into adds
                INSERT INTO "{}" (id, data) VALUES (OLD.id, NEW.data);
                
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;

            DROP TRIGGER IF EXISTS trg_update_{} ON "{}";
            CREATE TRIGGER trg_update_{}
            INSTEAD OF UPDATE ON "{}"
            FOR EACH ROW EXECUTE FUNCTION {}();
            "#,
            update_trigger_func, 
            adds,
            dels,
            adds,
            table_name, table_name,
            table_name, table_name,
            update_trigger_func
        );

        // DELETE trigger
        // Just add to x_dels
        // Also remove from x_adds?
        // If we only add to x_dels, it hides both Base and Adds (due to view logic).
        // BUT, keeping dead rows in x_adds is wasteful but safe.
        // However, cleaning x_adds is better.
        let delete_trigger_func = format!("func_delete_{}", table_name);
        let delete_sql = format!(
            r#"
            CREATE OR REPLACE FUNCTION {}() RETURNS TRIGGER AS $$
            BEGIN
                INSERT INTO "{}" (id) VALUES (OLD.id) ON CONFLICT (id) DO NOTHING;
                DELETE FROM "{}" WHERE id = OLD.id; -- Optional cleanup
                RETURN OLD;
            END;
            $$ LANGUAGE plpgsql;

            DROP TRIGGER IF EXISTS trg_delete_{} ON "{}";
            CREATE TRIGGER trg_delete_{}
            INSTEAD OF DELETE ON "{}"
            FOR EACH ROW EXECUTE FUNCTION {}();
            "#,
            delete_trigger_func,
            dels,
            adds,
            table_name, table_name,
            table_name, table_name,
            delete_trigger_func
        );

        client.batch_execute(&format!("{}{}{}", insert_sql, update_sql, delete_sql)).map_err(|e| {
             DBCenterError::QueryError(format!("Failed to create delta triggers: {}", e))
        })?;

        Ok(())
    }
}
