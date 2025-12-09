package main

import (
	"fmt"
	"log"
	"net/http"
	"os"
)

func main() {
	// Get the current working directory
	dir, err := os.Getwd()
	if err != nil {
		log.Fatal(err)
	}

	// Create a file server handler for the current directory
	fs := http.FileServer(http.Dir(dir))

	// Handle requests
	http.Handle("/", fs)

	port := "8080"
	fmt.Printf("Starting server at http://localhost:%s\n", port)
	fmt.Printf("Serving files from: %s\n", dir)
	fmt.Println("Press Ctrl+C to stop")

	// Start the server
	err = http.ListenAndServe(":"+port, nil)
	if err != nil {
		log.Fatal(err)
	}
}
