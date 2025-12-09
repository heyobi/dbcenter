package main

import "dbcenter"

func main() {
	db, _ := dbcenter.NewDBCenter("data", false)
	users := db.Collection("users")
	users.InsertOne(dbcenter.M{"name": "Alice", "age": 30})
}
