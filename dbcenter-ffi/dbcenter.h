#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
typedef struct CDBCenter CDBCenter;
typedef struct CVectorStore CVectorStore;


typedef struct CDBCenterResult {
  void *ptr;
  char *error;
} CDBCenterResult;

typedef struct CVectorStoreResult {
  void *ptr;
  char *error;
} CVectorStoreResult;

typedef struct CQueryResult {
  char **ids;
  uintptr_t num_results;
  double *distances;
  char **metadatas_json;
  char *error;
} CQueryResult;

/**
 * Create a new DBCenter instance.
 *
 * # Arguments
 * * `path` - Path to database directory (nullable for default)
 * * `persist` - 1 for persistent, 0 for volatile
 *
 * # Returns
 * CDBCenterResult with ptr to CDBCenter instance or error message.
 */
struct CDBCenterResult dbcenter_new(const char *path, uint8_t persist);

/**
 * Free the DBCenter instance.
 */
void dbcenter_free(struct CDBCenter *ptr);

/**
 * Free a string returned by library.
 */
void dbcenter_free_string(char *s);

/**
 * Install an extension (e.g., "pgvector").
 */
char *dbcenter_install_extension(struct CDBCenter *ptr, const char *name);

/**
 * Get a VectorStore instance.
 */
struct CVectorStoreResult dbcenter_get_vector_store(struct CDBCenter *ptr,
                                                    const char *name,
                                                    uintptr_t dimension);

/**
 * Free VectorStore
 */
void dbcenter_vector_store_free(struct CVectorStore *ptr);

/**
 * Add vector to store.
 * ids: char** (array of strings)
 * embeddings: float* (flat array of floats, size = count * dimension)
 * count: number of items
 */
char *dbcenter_vector_store_add(struct CVectorStore *ptr,
                                const char *const *ids,
                                const float *embeddings,
                                uintptr_t count);

/**
 * Query vector store.
 * query_embeddings: flat float array (usually just 1 vector)
 */
struct CQueryResult dbcenter_vector_store_query(struct CVectorStore *ptr,
                                                const float *query_embeddings,
                                                int64_t n_results);

/**
 * Free QueryResult
 */
void dbcenter_free_query_result(struct CQueryResult res);

/* Collection API */
typedef struct CCollection CCollection;

typedef struct CCollectionResult {
  void *ptr;
  char *error;
} CCollectionResult;

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
 * Returns NULL on success, or error message string on failure.
 */
char *dbcenter_collection_insert_one(struct CCollection *ptr, const char *json_doc);
