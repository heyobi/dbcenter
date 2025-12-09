# DBCenter Go Bindings

Go bindings for the DBCenter Rust library.

## Installation

1. Build the Rust library:
```bash
cd ../dbcenter-ffi
cargo build --release
```

2. Install the Go package:
```bash
go get github.com/heyobi/dbcenter-go
```

## Usage

```go
package main

import (
    "fmt"
    "github.com/heyobi/dbcenter-go"
)

func main() {
    // Create database
    db, err := dbcenter.New("./mydata", true)
    if err != nil {
        panic(err)
    }
    defer db.Close()

    // Get collection
    users := db.Collection("users")

    // Insert document
    id, err := users.InsertOne(map[string]interface{}{
        "name": "Ali",
        "age":  25,
        "city": "Istanbul",
    })
    if err != nil {
        panic(err)
    }
    fmt.Printf("Inserted document with ID: %d\n", id)

    // Find documents
    results, err := users.Find(map[string]interface{}{
        "age": map[string]interface{}{"$gt": 18},
    })
    if err != nil {
        panic(err)
    }

    for _, doc := range results {
        fmt.Printf("Found: %v\n", doc)
    }

    // Update
    err = users.UpdateOne(
        map[string]interface{}{"name": "Ali"},
        map[string]interface{}{"$set": map[string]interface{}{"age": 26}},
    )

    // Delete
    err = users.DeleteOne(map[string]interface{}{"name": "Ali"})

    // Install PostGIS
    err = db.InstallExtension("postgis")
}
```

## Environment

Make sure the Rust library is in your library path:

**Linux/macOS:**
```bash
export LD_LIBRARY_PATH=../target/release:$LD_LIBRARY_PATH
```

**Windows:**
```powershell
$env:PATH = "..\target\release;$env:PATH"
```

## Testing

```bash
go test -v
```
