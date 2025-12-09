package main

import (
	"fmt"
	"log"

	"github.com/heyobi/dbcenter-go"
)

func main() {
	fmt.Println("\n" + "======================================================================")
	fmt.Println("DBCenter Go API Test - Rust Edition")
	fmt.Println("======================================================================\n")

	// Create database
	fmt.Println("--- Creating database ---")
	db, err := dbcenter.New("./testdata_go", true)
	if err != nil {
		log.Fatalf("Failed to create database: %v", err)
	}
	defer db.Close()
	fmt.Println("✓ Database created\n")

	// Get collection
	users := db.Collection("users")

	// Insert document
	fmt.Println("--- Inserting document ---")
	id, err := users.InsertOne(map[string]interface{}{
		"name": "Ali",
		"age":  25,
		"city": "Istanbul",
	})
	if err != nil {
		log.Fatalf("Insert failed: %v", err)
	}
	fmt.Printf("✓ Inserted document with ID: %d\n\n", id)

	// Insert many
	fmt.Println("--- Inserting multiple documents ---")
	_, err = users.InsertOne(map[string]interface{}{
		"name": "Ayşe",
		"age":  30,
		"city": "Ankara",
	})
	if err != nil {
		log.Fatalf("Insert failed: %v", err)
	}
	_, err = users.InsertOne(map[string]interface{}{
		"name": "Mehmet",
		"age":  22,
		"city": "İzmir",
	})
	if err != nil {
		log.Fatalf("Insert failed: %v", err)
	}
	fmt.Println("✓ Multiple documents inserted\n")

	// Find all
	fmt.Println("--- Finding all documents ---")
	results, err := users.Find(nil)
	if err != nil {
		log.Fatalf("Find failed: %v", err)
	}
	fmt.Printf("✓ Found %d documents\n", len(results))
	for _, doc := range results {
		fmt.Printf("  • %v\n", doc)
	}
	fmt.Println()

	// Find with query
	fmt.Println("--- Finding with query (age > 24) ---")
	results, err = users.Find(map[string]interface{}{
		"age": map[string]interface{}{"$gt": 24},
	})
	if err != nil {
		log.Fatalf("Find failed: %v", err)
	}
	fmt.Printf("✓ Found %d documents\n", len(results))
	for _, doc := range results {
		fmt.Printf("  • %v\n", doc)
	}
	fmt.Println()

	// Count
	fmt.Println("--- Counting documents ---")
	count, err := users.Count(nil)
	if err != nil {
		log.Fatalf("Count failed: %v", err)
	}
	fmt.Printf("✓ Total documents: %d\n\n", count)

	// Update
	fmt.Println("--- Updating document ---")
	err = users.UpdateOne(
		map[string]interface{}{"name": "Ali"},
		map[string]interface{}{"$set": map[string]interface{}{"age": 26}},
	)
	if err != nil {
		log.Fatalf("Update failed: %v", err)
	}
	fmt.Println("✓ Document updated\n")

	// Delete one
	fmt.Println("--- Deleting one document ---")
	err = users.DeleteOne(map[string]interface{}{"name": "Mehmet"})
	if err != nil {
		log.Fatalf("Delete failed: %v", err)
	}
	fmt.Println("✓ Document deleted\n")

	// Count after delete
	count, err = users.Count(nil)
	if err != nil {
		log.Fatalf("Count failed: %v", err)
	}
	fmt.Printf("✓ Documents after delete: %d\n\n", count)

	// List collections
	fmt.Println("--- Listing collections ---")
	collections, err := db.ListCollections()
	if err != nil {
		log.Fatalf("List collections failed: %v", err)
	}
	fmt.Printf("✓ Collections: %v\n\n", collections)

	fmt.Println("======================================================================")
	fmt.Println("✓ ALL GO API TESTS PASSED!")
	fmt.Println("======================================================================\n")
}
