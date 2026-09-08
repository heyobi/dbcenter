#![deny(clippy::all)]

use dbcenter_core::DBCenter as CoreDBCenter;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::sync::Arc;
use tokio::runtime::Runtime;

#[napi]
pub struct DBCenter {
    inner: Arc<CoreDBCenter>,
    // we keep runtime if needed, but napi-rs usually manages async
}

#[napi]
impl DBCenter {
    /// Create a new DBCenter instance
    #[napi(constructor)]
    pub fn new(path: Option<String>, persist: Option<bool>) -> Result<Self> {
        let is_persist = persist.unwrap_or(false);

        // Use a blocking new for constructor because constructors in JS are synchronous usually,
        // though `napi` allows async factories.
        // Core DBCenter::new involves I/O.
        // Ideally we use a static runtime or block.
        let rt = Runtime::new().map_err(|e| Error::from_reason(e.to_string()))?;

        let db = rt
            .block_on(async {
                CoreDBCenter::new(path, is_persist, false, None, None, None, None, false)
            })
            .map_err(|e| Error::from_reason(e.to_string()))?;

        Ok(DBCenter { inner: db })
    }

    #[napi]
    pub fn install_extension(&self, name: String) -> Result<()> {
        self.inner
            .install_extension(&name)
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    #[napi]
    pub fn get_vector_store(&self, name: String, dimension: u32) -> Result<VectorStore> {
        let store = self
            .inner
            .vector_store(&name, dimension as usize)
            .map_err(|e| Error::from_reason(e.to_string()))?;

        // SAFETY: We keep `_parent` (Arc<DBCenter>) alive in the same struct.
        // The VectorStore references the data inside `_parent`.
        let store_static: dbcenter_core::vector_store::VectorStore<'static> =
            unsafe { std::mem::transmute(store) };

        Ok(VectorStore {
            inner: store_static,
            _parent: self.inner.clone(),
            dimension,
        })
    }
}

#[napi]
pub struct VectorStore {
    // We must cheat lifetime again or refactor core.
    // napi-rs structs must be 'static usually.
    // Core's VectorStore is VectorStore<'a>.
    // To safe keeping, we'll transmute to static but keep parent Arc alive.
    inner: dbcenter_core::vector_store::VectorStore<'static>,
    _parent: Arc<CoreDBCenter>,
    #[napi(readonly)]
    pub dimension: u32,
}

// Implement Send/Sync for VectorStore wrapper if needed by API
unsafe impl Send for VectorStore {}
unsafe impl Sync for VectorStore {}

#[napi]
impl VectorStore {
    #[napi]
    pub fn add(&self, ids: Vec<String>, embeddings: Vec<Vec<f64>>) -> Result<()> {
        // Core expects Vec<f32>. JS uses f64.
        let embeddings_f32: Vec<Vec<f32>> = embeddings
            .iter()
            .map(|v| v.iter().map(|&x| x as f32).collect())
            .collect();

        self.inner
            .add(&ids, &embeddings_f32, None)
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    #[napi]
    pub fn query(&self, query: Vec<f64>, n_results: i64) -> Result<Vec<QueryResult>> {
        let query_f32: Vec<f32> = query.iter().map(|&x| x as f32).collect();
        let results = self
            .inner
            .query(&[query_f32], n_results, None)
            .map_err(|e| Error::from_reason(e.to_string()))?;

        let mut js_results = Vec::new();
        for res in results {
            js_results.push(QueryResult {
                id: res["id"].as_str().unwrap_or("").to_string(),
                distance: res["distance"].as_f64().unwrap_or(0.0),
                metadata: res["metadata"].to_string(),
            });
        }
        Ok(js_results)
    }
}

#[napi(object)]
pub struct QueryResult {
    pub id: String,
    pub distance: f64,
    pub metadata: String,
}
