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
        
        // 1. Create Base Table
        let create_base_sql = format!(
            r#"CREATE TABLE IF NOT EXISTS "{}_base" (
                id UUID PRIMARY KEY,
                data JSONB,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            );"#,
            table_name
        );
        client.batch_execute(&create_base_sql).map_err(|e| DBCenterError::QueryError(format!("Failed to create base table: {}", e)))?;

        // 2. Create Adds Table
        let create_adds_sql = format!(
            r#"CREATE TABLE IF NOT EXISTS "{}_adds" (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                data JSONB,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            );"#,
            table_name
        );
        client.batch_execute(&create_adds_sql).map_err(|e| DBCenterError::QueryError(format!("Failed to create adds table: {}", e)))?;

        // 3. Create Dels Table
        let create_dels_sql = format!(
            r#"CREATE TABLE IF NOT EXISTS "{}_dels" (
                id UUID PRIMARY KEY,
                deleted_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
            );"#,
            table_name
        );
        client.batch_execute(&create_dels_sql).map_err(|e| DBCenterError::QueryError(format!("Failed to create dels table: {}", e)))?;

        // Create indexes
        let create_indexes_sql = format!(
            r#"
            CREATE INDEX IF NOT EXISTS idx_{}_base_data ON "{}_base" USING GIN (data);
            CREATE INDEX IF NOT EXISTS idx_{}_adds_data ON "{}_adds" USING GIN (data);
            "#,
            table_name, table_name,
            table_name, table_name
        );
        client.batch_execute(&create_indexes_sql).map_err(|e| DBCenterError::QueryError(format!("Failed to create indexes: {}", e)))?;

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
