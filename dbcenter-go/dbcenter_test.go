package dbcenter

import (
	"fmt"
	"math"
	"os"
	"testing"
	"time"
)

const (
	TestDBPath = "./go_test_db_final"
)

// TestSuite acts as the main entry point for the test suite
func TestDBCenterSuite(t *testing.T) {
	// Global Setup
	fmt.Println("============================================================")
	fmt.Println("Starting Professional DBCenter Go Test Suite")
	fmt.Println("============================================================")

	// Subtests
	t.Run("TestLifecycle", testLifecycle)
	t.Run("TestExtensions", testExtensions)
	t.Run("TestVectorStore", testVectorStore)

	// Global Teardown
	fmt.Println("============================================================")
	fmt.Println("Teardown DBCenter Go Test Suite")
	fmt.Println("============================================================")

	// Clean up concurrently or sequentially with logs
	cleanupPath(t, "./go_test_db_lifecycle")
	cleanupPath(t, "./go_test_db_ext")
	cleanupPath(t, "./go_test_db_vec")

	fmt.Println("Suite finished.")
}

func cleanupPath(t *testing.T, path string) {
	fmt.Printf("Cleaning up %s... ", path)

	done := make(chan error, 1)
	go func() {
		done <- ForceRemove(path)
	}()

	select {
	case err := <-done:
		if err != nil {
			fmt.Printf("Failed: %v (ignoring on Windows)\n", err)
		} else {
			fmt.Println("Success.")
		}
	case <-time.After(8 * time.Second):
		fmt.Println("Timeout (likely file lock, skipping).")
	}
}

func testLifecycle(t *testing.T) {
	path := "./go_test_db_lifecycle"
	fmt.Println("--- Test: Lifecycle ---")

	// Pre-clean
	_ = ForceRemove(path)

	// 1. Create Check
	db, err := NewDBCenter(path, true)
	if err != nil {
		t.Fatalf("Failed to create DBCenter: %v", err)
	}

	// Check if path exists
	if _, err := os.Stat(path); os.IsNotExist(err) {
		t.Errorf("Database directory was not created at %s", path)
	}

	// 2. Close Check
	db.Close()
	// Significant sleep to ensure Postgres process cleanup on Windows
	time.Sleep(1 * time.Second)
	fmt.Println("Lifecycle test passed.")
}

func testExtensions(t *testing.T) {
	path := "./go_test_db_ext"
	fmt.Println("--- Test: Extensions ---")

	_ = ForceRemove(path)

	db, err := NewDBCenter(path, true)
	if err != nil {
		t.Fatalf("Failed to open DB: %v", err)
	}
	defer func() {
		db.Close()
		time.Sleep(1 * time.Second)
	}()

	// Install pg_trgm (commonly available)
	err = db.InstallExtension("pg_trgm")
	if err != nil {
		t.Fatalf("Failed to install pg_trgm: %v", err)
	}
	fmt.Println("Extension 'pg_trgm' installed.")
}

func testVectorStore(t *testing.T) {
	path := "./go_test_db_vec"
	fmt.Println("--- Test: Vector Store ---")

	_ = ForceRemove(path)

	db, err := NewDBCenter(path, true)
	if err != nil {
		t.Fatalf("Failed to open DB: %v", err)
	}
	defer func() {
		db.Close()
		time.Sleep(1 * time.Second)
	}()

	// 1. Install pgvector (required for vector store)
	// Attempt to install, ignoring error if already exists/installed
	_ = db.InstallExtension("vector")

	// 2. Get Store
	storeName := "go_vectors"
	dim := 3
	store, err := db.GetVectorStore(storeName, dim)
	if err != nil {
		t.Fatalf("Failed to create VectorStore: %v", err)
	}
	defer store.Close()

	// 3. Add Vectors
	ids := []string{"v1", "v2", "v3"}
	embeddings := [][]float32{
		{1.0, 0.0, 0.0},
		{0.0, 1.0, 0.0},
		{0.0, 0.0, 1.0},
	}

	err = store.Add(ids, embeddings)
	if err != nil {
		t.Fatalf("Failed to add vectors: %v", err)
	}
	fmt.Printf("Added %d vectors.\n", len(ids))

	// 4. Query
	queryVec := []float32{1.0, 0.1, 0.1} // Should match v1 best
	results, err := store.Query(queryVec, 5)
	if err != nil {
		t.Fatalf("Vector query failed: %v", err)
	}

	if len(results) == 0 {
		t.Error("Expected results, got 0")
	} else {
		top := results[0]
		fmt.Printf("Top match: %s (Dist: %f)\n", top.ID, top.Distance)
		if top.ID != "v1" {
			t.Errorf("Expected top match 'v1', got '%s'", top.ID)
		}
		// Expect distance to be small (Cosine distance of similar vectors)
		// Or L2 distance. 1.0,0,0 vs 1.0,0.1,0.1 -> dist sq = 0 + 0.01 + 0.01 = 0.02. Sqrt ~ 0.14
		if top.Distance > 0.5 {
			t.Errorf("Distance seems too large for close match: %f", top.Distance)
		}
	}

	// Unused imports check
	_ = math.Abs(0) // Dummy usage
	fmt.Println("Vector Store test passed.")
}
