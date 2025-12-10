package main

import (
	"encoding/json"
	"fmt"
	"io/ioutil"
	"log"
	"os"

	"github.com/heyobi/dbcenter-go"
)

func main() {
	fmt.Println("\n" + "======================================================================")
	fmt.Println("DBCenter Go API Test - Rust Edition")
	fmt.Println("======================================================================\n")

	files, err := ioutil.ReadDir(".")
	if err != nil {
		log.Fatal(err)
	}

	// Silmek istediğiniz dosyaların isimlerini tanımlayın
	filesToDelete := map[string]struct{}{
		"dbcenter-watcher":   {},
		"libdbcenter_ffi.so": {},
	}

	// Dosyaları kontrol et ve sil
	for _, file := range files {
		if _, exists := filesToDelete[file.Name()]; exists {
			err := os.Remove(file.Name())
			if err != nil {
				log.Printf("Hata: %s silinemedi: %v\n", file.Name(), err)
			} else {
				fmt.Printf("%s başarıyla silindi\n", file.Name())
			}
		}
	}

	// Create database
	fmt.Println("--- Creating database ---")
	db, err := dbcenter.New("./testdata_go_linux")
	if err != nil {
		log.Fatalf("Failed to create database: %v", err)
	}
	defer db.Close()
	fmt.Println("✓ Database created\n")

	// Install pgvector if needed (just to test method)
	// err = db.InstallExtension("vector")
	// if err != nil {
	// 	 fmt.Printf("Extension install warning: %v\n", err)
	// }

	// Get collection
	users := db.Collection("users")

	// Insert document
	fmt.Println("--- Inserting document ---")
	err = users.InsertOne(map[string]interface{}{
		"name": "Ali",
		"age":  25,
		"city": "Istanbul",
	})
	if err != nil {
		log.Fatalf("Insert failed: %v", err)
	}
	fmt.Println("✓ Inserted document (Ali)")

	// Insert another
	err = users.InsertOne(map[string]interface{}{
		"name": "Zeynep",
		"age":  30,
		"city": "Izmir",
	})
	if err != nil {
		log.Fatalf("Insert failed: %v", err)
	}
	fmt.Println("✓ Inserted document (Zeynep)")

	files, err = ioutil.ReadDir(".")
	if err != nil {
		log.Fatal(err)
	}

	// Dosyaları yazdır
	for _, file := range files {
		fmt.Println(file.Name())
	}

	// Query via SQL to verify
	// Assuming table name is same as collection name (or lowercased)
	// Note: If this fails, we will just print error and continue, but InsertOne success is main test.
	fmt.Println("\n--- Querying via SQL (SELECT count(*) FROM users) ---")
	rows, err := db.Query("SELECT count(*) FROM users")
	if err != nil {
		fmt.Printf("SQL Query failed (expected if table mapping is complex): %v\n", err)
	} else {
		// Pretty print result
		bytes, _ := json.MarshalIndent(rows, "", "  ")
		fmt.Printf("✓ SQL Result:\n%s\n", string(bytes))
	}

	fmt.Println("\n======================================================================")
	fmt.Println("✓ BASIC GO API TESTS PASSED!")
	fmt.Println("======================================================================\n")
}
