use crate::error::{DBCenterError, Result};
use crate::query::quote_ident;
use crate::DBCenter;

/// The column a delta-mode collection is partitioned on: `'base'` for rows
/// that have been synced, `'adds'` for rows waiting to be. It is bookkeeping,
/// not part of the document, so it is never synced and never a field name.
pub const LAYER_COLUMN: &str = "_layer";

/// Set (with `SET LOCAL`) by the sync engine while it writes rows that came
/// from or are going to object storage. The triggers below exist to turn a
/// local edit into a pending change; a row being replicated is not one.
pub const REPLICATING: &str = "SET LOCAL dbcenter.replicating = on";

pub struct DeltaManager;

impl DeltaManager {
    /// Make sure `table_name` exists as a delta-mode collection.
    ///
    /// The collection is a table partitioned on [`LAYER_COLUMN`], with
    /// `<name>_base` and `<name>_adds` as its two partitions and `<name>_dels`
    /// beside it holding the ids deleted here.
    ///
    /// It used to be a view: `_adds UNION ALL` the rows of `_base` with no
    /// newer version in `_adds` and no marker in `_dels`. PostgreSQL cannot
    /// push an ORDER BY ... LIMIT through the anti-join arm, so a hundred rows
    /// by timestamp sorted the whole collection - 256.9 ms against 0.16 ms for
    /// the partitioned shape on the same 200,000 rows and the same index
    /// (`scripts/shape-experiment`). Partitions give the planner a merge
    /// append over two index scans that stops when it has enough rows.
    ///
    /// What partitions do not give is a unique id across the two of them: the
    /// key of a partitioned table has to include the partition column. Where
    /// the view showed the `_adds` version of an id present in both, the table
    /// would show both. So an id lives in exactly one partition, and each
    /// path that could put it in the second one moves it instead:
    ///
    /// - an update through the collection moves the row from `_base` to
    ///   `_adds` (a trigger on `_base` sets the layer, and PostgreSQL moves it);
    /// - an insert into the collection with an id that is in `_base` takes
    ///   that row out of `_base`, within the same statement;
    /// - a restore skips an id that is pending in `_adds` or deleted here.
    ///
    /// The first two are triggers so that a client on the wire protocol gets
    /// them too. Writing to `_adds` by name skips the second one, which is how
    /// `insert_one` avoids paying for it: `_adds` is DBCenter's own, and
    /// nothing that writes to it directly passes an id. A deletion leaves the id in `_dels`, which nothing reads any
    /// more except the restore, so a pull cannot bring the row back.
    ///
    /// A collection still shaped as a view is converted in place, once.
    pub fn ensure_delta_tables(db: &DBCenter, table_name: &str) -> Result<()> {
        let mut client = db.get_client();
        let lookup_failed = |e: postgres::Error| {
            DBCenterError::QueryError(format!(
                "Could not set up delta collection '{}': {}",
                table_name,
                crate::utils::format_postgres_error(&e)
            ))
        };

        // Opening a collection that is already partitioned is the common case
        // and costs one catalogue lookup.
        if shape(&mut *client, table_name).map_err(lookup_failed)? == Some('p') {
            return Ok(());
        }

        // Two callers opening the same new collection at once would both try
        // to create it. The lock serialises them; the second one then finds
        // the work done.
        let mut transaction = client.transaction().map_err(lookup_failed)?;
        transaction
            .execute(
                "SELECT pg_advisory_xact_lock(hashtext('dbcenter.delta.' || $1::text))",
                &[&table_name],
            )
            .map_err(lookup_failed)?;

        let ddl = match shape(&mut transaction, table_name).map_err(lookup_failed)? {
            Some('p') => return Ok(()),
            Some('v') => {
                tracing::info!(
                    "Converting collection '{}' from a view to a partitioned table",
                    table_name
                );
                convert_sql(table_name)
            }
            None => create_sql(table_name),
            Some(other) => {
                return Err(DBCenterError::QueryError(format!(
                    "'{}' already exists and is not a delta collection (relkind '{}')",
                    table_name, other
                )))
            }
        };

        transaction
            .batch_execute(&format!(
                "{}\n{}\n{}",
                UUID_DEFAULT_SQL,
                ddl,
                triggers_sql(table_name)
            ))
            .map_err(lookup_failed)?;
        transaction.commit().map_err(lookup_failed)?;
        Ok(())
    }
}

impl DeltaManager {
    /// Convert every collection still shaped as the old view.
    ///
    /// A failure is logged rather than returned. An unconverted collection
    /// still answers as before, only without the ordered-read speedup, and
    /// refusing to open the database over it would lock the caller out of
    /// every other collection too.
    pub fn convert_view_collections(db: &DBCenter) {
        let names: Vec<String> = {
            let mut client = db.get_client();
            match client.query(
                "SELECT c.relname::text FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = current_schema() AND c.relkind = 'v' \
                   AND to_regclass(quote_ident(c.relname || '_base')) IS NOT NULL \
                   AND to_regclass(quote_ident(c.relname || '_adds')) IS NOT NULL",
                &[],
            ) {
                Ok(rows) => rows.iter().map(|r| r.get(0)).collect(),
                Err(e) => {
                    tracing::warn!(
                        "Could not look for collections to convert: {}",
                        crate::utils::format_postgres_error(&e)
                    );
                    return;
                }
            }
            // The connection goes back here: converting takes one of its own.
        };
        for name in names {
            if let Err(e) = Self::ensure_delta_tables(db, &name) {
                tracing::warn!(
                    "Collection '{}' is still a view and reads will not use its \
                     indexes for ordering: {}",
                    name,
                    e
                );
            }
        }
    }
}

/// What `table_name` currently is: `'p'` for a partitioned collection, `'v'`
/// for the old view shape, `None` if there is nothing yet.
fn shape(
    client: &mut impl postgres::GenericClient,
    table_name: &str,
) -> std::result::Result<Option<char>, postgres::Error> {
    let row = client.query_one(
        "SELECT (SELECT relkind::text FROM pg_class WHERE oid = to_regclass($1)), \
                to_regclass($2) IS NOT NULL",
        &[
            &quote_ident(table_name),
            &quote_ident(&format!("{}_base", table_name)),
        ],
    )?;
    let kind: Option<String> = row.get(0);
    let has_base: bool = row.get(1);
    // A `_base` with no collection over it is the old shape with its view
    // gone, and converts the same way.
    Ok(kind
        .and_then(|k| k.chars().next())
        .or(has_base.then_some('v')))
}

fn names(table_name: &str) -> (String, String, String, String) {
    (
        quote_ident(table_name),
        quote_ident(&format!("{}_base", table_name)),
        quote_ident(&format!("{}_adds", table_name)),
        quote_ident(&format!("{}_dels", table_name)),
    )
}

/// `gen_random_uuid()`, the id default, is built in from PostgreSQL 13. On 12
/// it comes from pgcrypto, which every PostgreSQL build ships but nothing
/// creates, so the first delta collection on 12 failed without this.
const UUID_DEFAULT_SQL: &str = "DO $$ BEGIN
    IF current_setting('server_version_num')::int < 130000 THEN
        CREATE EXTENSION IF NOT EXISTS pgcrypto;
    END IF;
END $$;";

fn create_sql(table_name: &str) -> String {
    let (coll, base, adds, dels) = names(table_name);
    let data_index = quote_ident(&format!("idx_{}_data", table_name));
    format!(
        r#"
        CREATE TABLE IF NOT EXISTS {dels} (
            id UUID PRIMARY KEY,
            deleted_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE {coll} (
            id UUID NOT NULL DEFAULT gen_random_uuid(),
            data JSONB,
            created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
            {layer} TEXT NOT NULL DEFAULT 'adds'
        ) PARTITION BY LIST ({layer});
        CREATE TABLE {base} PARTITION OF {coll} (
            PRIMARY KEY (id),
            {layer} DEFAULT 'base'
        ) FOR VALUES IN ('base');
        CREATE TABLE {adds} PARTITION OF {coll} (
            PRIMARY KEY (id)
        ) FOR VALUES IN ('adds');
        CREATE INDEX {data_index} ON {coll} USING GIN (data);
        "#,
        layer = quote_ident(LAYER_COLUMN),
    )
}

/// From the view over `_base`, `_adds` and `_dels` to the partitioned table.
///
/// The rows the view was hiding - old versions in `_base` of ids now in
/// `_adds`, and rows marked deleted in `_dels` - are removed from `_base`, so
/// the table shows exactly what the view did and no id is in both partitions.
/// `_dels` itself is kept: the restore still reads it.
fn convert_sql(table_name: &str) -> String {
    let (coll, base, adds, dels) = names(table_name);
    let data_index = quote_ident(&format!("idx_{}_data", table_name));
    // The old trigger functions were created with unquoted names, which
    // PostgreSQL folds to lower case.
    let old_functions = ["insert", "update", "delete"]
        .iter()
        .map(|op| {
            format!(
                "DROP FUNCTION IF EXISTS {}();",
                quote_ident(&format!("func_{}_{}", op, table_name).to_lowercase())
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"
        DROP VIEW IF EXISTS {coll};
        {old_functions}
        CREATE TABLE IF NOT EXISTS {dels} (
            id UUID PRIMARY KEY,
            deleted_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
        );
        DELETE FROM {base} b
        WHERE EXISTS (SELECT 1 FROM {adds} a WHERE a.id = b.id)
           OR EXISTS (SELECT 1 FROM {dels} d WHERE d.id = b.id);
        ALTER TABLE {base} ADD COLUMN {layer} TEXT NOT NULL DEFAULT 'base';
        ALTER TABLE {adds} ADD COLUMN {layer} TEXT NOT NULL DEFAULT 'adds';
        CREATE TABLE {coll} (LIKE {adds} INCLUDING DEFAULTS) PARTITION BY LIST ({layer});
        ALTER TABLE {coll} ATTACH PARTITION {base} FOR VALUES IN ('base');
        ALTER TABLE {coll} ATTACH PARTITION {adds} FOR VALUES IN ('adds');
        CREATE INDEX IF NOT EXISTS {data_index} ON {coll} USING GIN (data);
        "#,
        layer = quote_ident(LAYER_COLUMN),
    )
}

fn triggers_sql(table_name: &str) -> String {
    let (coll, base, adds, dels) = names(table_name);
    let function = |what: &str| quote_ident(&format!("dbcenter_{}_{}", what, table_name));
    let trigger = |what: &str| quote_ident(&format!("dbcenter_{}", what));
    let replicating = "current_setting('dbcenter.replicating', true) = 'on'";
    format!(
        r#"
        -- An update of a synced row is a pending change again: send it to
        -- _adds. Setting the layer is enough; PostgreSQL moves the row.
        CREATE OR REPLACE FUNCTION {route_fn}() RETURNS TRIGGER AS $$
        BEGIN
            IF {replicating} THEN RETURN NEW; END IF;
            NEW.{layer} := 'adds';
            RETURN NEW;
        END;
        $$ LANGUAGE plpgsql;
        DROP TRIGGER IF EXISTS {route} ON {base};
        CREATE TRIGGER {route} BEFORE UPDATE ON {base}
            FOR EACH ROW EXECUTE FUNCTION {route_fn}();

        -- A pending row whose id is already in _base replaces it. A client
        -- writing SQL can pass an id; insert_one never does, and it writes to
        -- _adds by name, which a statement trigger on the collection does not
        -- see - so it pays nothing for this. Once per statement, over the rows
        -- it inserted: as a row trigger this doubled the cost of an insert.
        CREATE OR REPLACE FUNCTION {claim_fn}() RETURNS TRIGGER AS $$
        BEGIN
            IF {replicating} THEN RETURN NULL; END IF;
            DELETE FROM {base} b USING inserted i WHERE b.id = i.id;
            RETURN NULL;
        END;
        $$ LANGUAGE plpgsql;
        DROP TRIGGER IF EXISTS {claim} ON {coll};
        CREATE TRIGGER {claim} AFTER INSERT ON {coll}
            REFERENCING NEW TABLE AS inserted
            FOR EACH STATEMENT EXECUTE FUNCTION {claim_fn}();

        -- Remember what was deleted here, so a restore does not bring it back.
        CREATE OR REPLACE FUNCTION {tomb_fn}() RETURNS TRIGGER AS $$
        BEGIN
            IF {replicating} THEN RETURN OLD; END IF;
            INSERT INTO {dels} (id) VALUES (OLD.id) ON CONFLICT (id) DO NOTHING;
            RETURN OLD;
        END;
        $$ LANGUAGE plpgsql;
        DROP TRIGGER IF EXISTS {tomb} ON {base};
        CREATE TRIGGER {tomb} AFTER DELETE ON {base}
            FOR EACH ROW EXECUTE FUNCTION {tomb_fn}();
        DROP TRIGGER IF EXISTS {tomb} ON {adds};
        CREATE TRIGGER {tomb} AFTER DELETE ON {adds}
            FOR EACH ROW EXECUTE FUNCTION {tomb_fn}();
        "#,
        layer = quote_ident(LAYER_COLUMN),
        route_fn = function("route"),
        route = trigger("route"),
        claim_fn = function("claim"),
        claim = trigger("claim"),
        tomb_fn = function("tomb"),
        tomb = trigger("tomb"),
    )
}
