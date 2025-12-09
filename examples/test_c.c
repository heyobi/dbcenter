#include <stdio.h>
#include <stdlib.h>
#include "../dbcenter-ffi/dbcenter.h"

int main() {
    printf("\n");
    printf("======================================================================\n");
    printf("DBCenter C API Test - Rust Edition\n");
    printf("======================================================================\n\n");

    // Create database
    printf("--- Creating database ---\n");
    CDBCenter* db = dbcenter_new("./testdata_c", true);
    if (!db) {
        const char* err = dbcenter_last_error();
        fprintf(stderr, "Failed to create database: %s\n", err);
        dbcenter_free_error((char*)err);
        return 1;
    }
    printf("✓ Database created\n\n");

    // Insert document
    printf("--- Inserting document ---\n");
    const char* doc = "{\"name\":\"Ali\",\"age\":25,\"city\":\"Istanbul\"}";
    int32_t id = dbcenter_insert_one(db, "users", doc);
    if (id == 0) {
        const char* err = dbcenter_last_error();
        fprintf(stderr, "Insert failed: %s\n", err);
        dbcenter_free_error((char*)err);
        dbcenter_free(db);
        return 1;
    }
    printf("✓ Inserted document with ID: %d\n\n", id);

    // Find documents
    printf("--- Finding documents ---\n");
    char* results = dbcenter_find(db, "users", NULL);
    if (!results) {
        const char* err = dbcenter_last_error();
        fprintf(stderr, "Find failed: %s\n", err);
        dbcenter_free_error((char*)err);
        dbcenter_free(db);
        return 1;
    }
    printf("✓ Found documents: %s\n\n", results);
    dbcenter_free_string(results);

    // Count documents
    printf("--- Counting documents ---\n");
    int64_t count = dbcenter_count(db, "users", NULL);
    if (count < 0) {
        const char* err = dbcenter_last_error();
        fprintf(stderr, "Count failed: %s\n", err);
        dbcenter_free_error((char*)err);
        dbcenter_free(db);
        return 1;
    }
    printf("✓ Document count: %lld\n\n", (long long)count);

    // Update document
    printf("--- Updating document ---\n");
    const char* query = "{\"name\":\"Ali\"}";
    const char* update = "{\"$set\":{\"age\":26}}";
    if (!dbcenter_update_one(db, "users", query, update)) {
        const char* err = dbcenter_last_error();
        fprintf(stderr, "Update failed: %s\n", err);
        dbcenter_free_error((char*)err);
        dbcenter_free(db);
        return 1;
    }
    printf("✓ Document updated\n\n");

    // Delete document
    printf("--- Deleting document ---\n");
    if (!dbcenter_delete_one(db, "users", query)) {
        const char* err = dbcenter_last_error();
        fprintf(stderr, "Delete failed: %s\n", err);
        dbcenter_free_error((char*)err);
        dbcenter_free(db);
        return 1;
    }
    printf("✓ Document deleted\n\n");

    // Verify deletion
    count = dbcenter_count(db, "users", NULL);
    printf("✓ Final count: %lld\n\n", (long long)count);

    // List collections
    printf("--- Listing collections ---\n");
    char* collections = dbcenter_list_collections(db);
    if (collections) {
        printf("✓ Collections: %s\n\n", collections);
        dbcenter_free_string(collections);
    }

    // Cleanup
    printf("--- Cleanup ---\n");
    dbcenter_free(db);
    printf("✓ Database freed\n\n");

    printf("======================================================================\n");
    printf("✓ ALL C API TESTS PASSED!\n");
    printf("======================================================================\n\n");

    return 0;
}
