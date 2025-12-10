#ifndef DBCENTER_H
#define DBCENTER_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/* Forward declarations */
typedef struct CDBCenter CDBCenter;
typedef struct CVectorStore CVectorStore;
typedef struct CCollection CCollection;

/* Result types */
typedef struct CDBCenterResult {
    void *ptr;
    char *error;
} CDBCenterResult;

typedef struct CVectorStoreResult {
    void *ptr;
    char *error;
} CVectorStoreResult;

typedef struct CCollectionResult {
    void *ptr;
    char *error;
} CCollectionResult;

typedef struct CQueryResult {
    char **ids;
    uintptr_t num_results;
    double *distances;
    char **metadatas_json;
    char *error;
} CQueryResult;

typedef struct CJsonResult {
    char *json;
    char *error;
} CJsonResult;

/* ============================================================================
 * DBCenter Core Functions
 * ============================================================================ */

/**
 * Create a new DBCenter instance.
 *
 * @param path Path to database directory (NULL for temporary)
 * @param persist 1 for persistent, 0 for volatile
 * @return CDBCenterResult with ptr to CDBCenter instance or error message
 */
struct CDBCenterResult dbcenter_new(const char *path, uint8_t persist);

/**
 * Free the DBCenter instance.
 */
void dbcenter_free(struct CDBCenter *ptr);

/**
 * Shutdown the DBCenter instance explicitly.
 * @return NULL on success, error message on failure
 */
char *dbcenter_shutdown(struct CDBCenter *ptr);

/**
 * Free a string returned by library.
 */
void dbcenter_free_string(char *s);

/**
 * Force remove a database directory.
 * @return NULL on success, error message on failure
 */
char *dbcenter_force_remove(const char *path);

/**
 * Install an extension (e.g., "pgvector").
 * @return NULL on success, error message on failure
 */
char *dbcenter_install_extension(struct CDBCenter *ptr, const char *name);

/**
 * Execute a raw SQL query and return results as JSON.
 */
struct CJsonResult dbcenter_sql_query(struct CDBCenter *ptr, const char *sql);

/* ============================================================================
 * Collection Functions
 * ============================================================================ */

/**
 * Get a Collection instance.
 */
struct CCollectionResult dbcenter_get_collection(struct CDBCenter *ptr, const char *name);

/**
 * Free Collection.
 */
void dbcenter_collection_free(struct CCollection *ptr);

/**
 * Insert a document (JSON string).
 * @return NULL on success, error message on failure
 */
char *dbcenter_collection_insert_one(struct CCollection *ptr, const char *json_doc);

/* ============================================================================
 * VectorStore Functions
 * ============================================================================ */

/**
 * Get a VectorStore instance.
 */
struct CVectorStoreResult dbcenter_get_vector_store(struct CDBCenter *ptr,
                                                    const char *name,
                                                    uintptr_t dimension);

/**
 * Free VectorStore.
 */
void dbcenter_vector_store_free(struct CVectorStore *ptr);

/**
 * Add vectors to store.
 * @param ids Array of string IDs
 * @param embeddings Flat array of floats (size = count * dimension)
 * @param count Number of vectors
 * @return NULL on success, error message on failure
 */
char *dbcenter_vector_store_add(struct CVectorStore *ptr,
                                const char *const *ids,
                                const float *embeddings,
                                uintptr_t count);

/**
 * Query vector store.
 * @param query_embeddings Flat float array (single query vector)
 * @param n_results Number of results to return
 */
struct CQueryResult dbcenter_vector_store_query(struct CVectorStore *ptr,
                                                const float *query_embeddings,
                                                int64_t n_results);

/**
 * Free QueryResult.
 */
void dbcenter_free_query_result(struct CQueryResult res);

#endif /* DBCENTER_H */
