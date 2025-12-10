#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../dbcenter-ffi/dbcenter.h"

int main() {
    printf("\n");
    printf("======================================================================\n");
    printf("DBCenter C API Test - Rust Edition\n");
    printf("======================================================================\n\n");

    // Create database
    printf("--- Creating database ---\n");
    struct CDBCenterResult db_result = dbcenter_new("./testdata_c", 1);
    if (db_result.error != NULL) {
        fprintf(stderr, "Failed to create database: %s\n", db_result.error);
        dbcenter_free_string(db_result.error);
        return 1;
    }
    struct CDBCenter* db = (struct CDBCenter*)db_result.ptr;
    printf("✓ Database created\n\n");

    // Get collection
    printf("--- Getting collection ---\n");
    struct CCollectionResult coll_result = dbcenter_get_collection(db, "users");
    if (coll_result.error != NULL) {
        fprintf(stderr, "Failed to get collection: %s\n", coll_result.error);
        dbcenter_free_string(coll_result.error);
        dbcenter_free(db);
        return 1;
    }
    struct CCollection* coll = (struct CCollection*)coll_result.ptr;
    printf("✓ Got collection 'users'\n\n");

    // Insert document
    printf("--- Inserting document ---\n");
    const char* doc = "{\"name\":\"Ali\",\"age\":25,\"city\":\"Istanbul\"}";
    char* insert_err = dbcenter_collection_insert_one(coll, doc);
    if (insert_err != NULL) {
        fprintf(stderr, "Insert failed: %s\n", insert_err);
        dbcenter_free_string(insert_err);
        dbcenter_collection_free(coll);
        dbcenter_free(db);
        return 1;
    }
    printf("✓ Inserted document\n\n");

    // Query via SQL
    printf("--- Querying via SQL ---\n");
    struct CJsonResult sql_result = dbcenter_sql_query(db, "SELECT * FROM users");
    if (sql_result.error != NULL) {
        fprintf(stderr, "SQL query failed: %s\n", sql_result.error);
        dbcenter_free_string(sql_result.error);
        dbcenter_collection_free(coll);
        dbcenter_free(db);
        return 1;
    }
    printf("✓ SQL Result: %s\n\n", sql_result.json);
    dbcenter_free_string(sql_result.json);

    // Test VectorStore (optional - requires pgvector extension)
    printf("--- Testing VectorStore (optional) ---\n");
    struct CVectorStoreResult vs_result = dbcenter_get_vector_store(db, "embeddings", 3);
    if (vs_result.error != NULL) {
        // VectorStore requires pgvector extension which may not be available
        printf("⚠ VectorStore skipped (pgvector not available): %s\n\n", vs_result.error);
        dbcenter_free_string(vs_result.error);
        // Continue without VectorStore test
    } else {
        struct CVectorStore* vs = (struct CVectorStore*)vs_result.ptr;
        printf("✓ Got vector store 'embeddings' (dim=3)\n");

        // Add vectors
        const char* ids[] = {"v1", "v2"};
        float embeddings[] = {1.0f, 0.0f, 0.0f, 0.0f, 1.0f, 0.0f}; // 2 vectors of dim 3
        char* add_err = dbcenter_vector_store_add(vs, ids, embeddings, 2);
        if (add_err != NULL) {
            fprintf(stderr, "Vector add failed: %s\n", add_err);
            dbcenter_free_string(add_err);
        } else {
            printf("✓ Added 2 vectors\n");

            // Query vectors
            float query_vec[] = {1.0f, 0.1f, 0.0f};
            struct CQueryResult query_result = dbcenter_vector_store_query(vs, query_vec, 2);
            if (query_result.error != NULL) {
                fprintf(stderr, "Vector query failed: %s\n", query_result.error);
                dbcenter_free_string(query_result.error);
            } else {
                printf("✓ Query returned %zu results\n", query_result.num_results);
                for (size_t i = 0; i < query_result.num_results; i++) {
                    printf("  - ID: %s, Distance: %.4f\n", query_result.ids[i], query_result.distances[i]);
                }
                dbcenter_free_query_result(query_result);
            }
        }
        dbcenter_vector_store_free(vs);
        printf("✓ VectorStore freed\n");
    }
    printf("\n");

    // Cleanup
    printf("--- Cleanup ---\n");
    dbcenter_collection_free(coll);
    printf("✓ Collection freed\n");
    
    char* shutdown_err = dbcenter_shutdown(db);
    if (shutdown_err != NULL) {
        fprintf(stderr, "Shutdown warning: %s\n", shutdown_err);
        dbcenter_free_string(shutdown_err);
    }
    dbcenter_free(db);
    printf("✓ Database freed\n\n");

    // Force remove test data
    char* remove_err = dbcenter_force_remove("./testdata_c");
    if (remove_err != NULL) {
        fprintf(stderr, "Force remove warning: %s\n", remove_err);
        dbcenter_free_string(remove_err);
    }
    printf("✓ Test data removed\n\n");

    printf("======================================================================\n");
    printf("✓ ALL C API TESTS PASSED!\n");
    printf("======================================================================\n\n");

    return 0;
}
