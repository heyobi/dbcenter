package dbcenter

/*
#cgo CFLAGS: -I${SRCDIR}/include
#cgo LDFLAGS: -L${SRCDIR} -ldbcenter_ffi
#include "dbcenter.h"
#include <stdlib.h> // For C.free
*/
import "C"
import (
	"encoding/json"
	"errors"
	"fmt"
	"runtime"
	"unsafe"
)

// M is a shortcut for map[string]interface{}
type M map[string]interface{}

// DBCenter is the main database instance
type DBCenter struct {
	ptr *C.CDBCenter
}

// VectorStore is the vector storage interface
type VectorStore struct {
	ptr *C.CVectorStore
}

// ForceRemove forcefully removes a database directory
func ForceRemove(path string) error {
	cPath := C.CString(path)
	defer C.free(unsafe.Pointer(cPath))

	errPtr := C.dbcenter_force_remove(cPath)
	if errPtr != nil {
		defer C.dbcenter_free_string(errPtr)
		return errors.New(C.GoString(errPtr))
	}
	return nil
}

// NewDBCenter creates a new database instance
func NewDBCenter(path string, persist bool) (*DBCenter, error) {
	cPath := C.CString(path)
	defer C.free(unsafe.Pointer(cPath))

	cPersist := C.uchar(0)
	if persist {
		cPersist = 1
	}

	result := C.dbcenter_new(cPath, cPersist)

	if result.error != nil {
		defer C.dbcenter_free_string(result.error)
		return nil, errors.New(C.GoString(result.error))
	}

	db := &DBCenter{ptr: (*C.CDBCenter)(result.ptr)}

	// Set finalizer to ensure cleanup if Close() is forgotten
	runtime.SetFinalizer(db, func(d *DBCenter) {
		d.Close()
	})

	return db, nil
}

// New is an alias for NewDBCenter
func New(path string) (*DBCenter, error) {
	return NewDBCenter(path, true)
}

// Query executes a raw SQL query
func (db *DBCenter) Query(sql string) ([]interface{}, error) {
	cSql := C.CString(sql)
	defer C.free(unsafe.Pointer(cSql))

	res := C.dbcenter_sql_query(db.ptr, cSql)

	if res.error != nil {
		msg := C.GoString(res.error)
		C.dbcenter_free_string(res.error)
		return nil, errors.New(msg)
	}

	defer C.dbcenter_free_string(res.json)
	jsonStr := C.GoString(res.json)

	var results []interface{}
	if err := json.Unmarshal([]byte(jsonStr), &results); err != nil {
		return nil, fmt.Errorf("failed to parse query result: %w", err)
	}

	return results, nil
}

// Close closes the database and ensures shutdown
func (db *DBCenter) Close() error {
	if db.ptr != nil {
		// Explicit shutdown first
		errPtr := C.dbcenter_shutdown(db.ptr)
		var err error
		if errPtr != nil {
			defer C.dbcenter_free_string(errPtr)
			err = errors.New(C.GoString(errPtr))
		}

		// Always free the wrapper
		C.dbcenter_free(db.ptr)
		db.ptr = nil
		return err
	}
	return nil
}

// InstallExtension installs a PostgreSQL extension
func (db *DBCenter) InstallExtension(name string) error {
	cName := C.CString(name)
	defer C.free(unsafe.Pointer(cName))

	errPtr := C.dbcenter_install_extension(db.ptr, cName)
	if errPtr != nil {
		defer C.dbcenter_free_string(errPtr)
		return errors.New(C.GoString(errPtr))
	}
	return nil
}

// GetVectorStore retrieves a vector store
func (db *DBCenter) GetVectorStore(name string, dimension int) (*VectorStore, error) {
	cName := C.CString(name)
	defer C.free(unsafe.Pointer(cName))

	result := C.dbcenter_get_vector_store(db.ptr, cName, C.size_t(dimension))

	if result.error != nil {
		defer C.dbcenter_free_string(result.error)
		return nil, errors.New(C.GoString(result.error))
	}

	return &VectorStore{ptr: (*C.CVectorStore)(result.ptr)}, nil
}

// Close closes the vector store
func (vs *VectorStore) Close() {
	if vs.ptr != nil {
		C.dbcenter_vector_store_free(vs.ptr)
		vs.ptr = nil
	}
}

// Add adds vectors to the store
func (vs *VectorStore) Add(ids []string, embeddings [][]float32) error {
	count := len(ids)
	if count != len(embeddings) {
		return errors.New("ids and embeddings length mismatch")
	}

	// Prepare C arrays
	cIds := make([]*C.char, count)
	for i, id := range ids {
		cIds[i] = C.CString(id)
		defer C.free(unsafe.Pointer(cIds[i]))
	}

	// Flatten embeddings
	dim := len(embeddings[0]) // Assume uniform
	flatEmbeddings := make([]C.float, count*dim)
	for i, vec := range embeddings {
		for j, val := range vec {
			flatEmbeddings[i*dim+j] = C.float(val)
		}
	}

	errPtr := C.dbcenter_vector_store_add(
		vs.ptr,
		&cIds[0],
		(*C.float)(&flatEmbeddings[0]),
		C.size_t(count),
	)

	if errPtr != nil {
		defer C.dbcenter_free_string(errPtr)
		return errors.New(C.GoString(errPtr))
	}
	return nil
}

// QueryResult represents a search result
type QueryResult struct {
	ID       string
	Distance float64
	Metadata string
}

// Query searches correctly
func (vs *VectorStore) Query(query []float32, nResults int) ([]QueryResult, error) {
	if len(query) == 0 {
		return nil, errors.New("empty query vector")
	}

	cQuery := make([]C.float, len(query))
	for i, v := range query {
		cQuery[i] = C.float(v)
	}

	res := C.dbcenter_vector_store_query(
		vs.ptr,
		(*C.float)(&cQuery[0]),
		C.int64_t(nResults),
	)

	if res.error != nil {
		defer C.dbcenter_free_string(res.error)
		// Also free result shell? C API says dbcenter_free_query_result handles all.
		// But in error case, if internals are null, it's fine.
		// Wait, my C logic allocates CQueryResult with error string.
		// I should call the free function for the whole struct.
		// But I need the error message first.
		msg := C.GoString(res.error)
		C.dbcenter_free_query_result(res)
		return nil, errors.New(msg)
	}
	defer C.dbcenter_free_query_result(res)

	results := make([]QueryResult, res.num_results)

	// Iterate pointers
	// Go CGO prohibits pointer arithmetic, so we cast to large array
	// access via slice
	idsSlice := (*[1 << 30]*C.char)(unsafe.Pointer(res.ids))[:res.num_results:res.num_results]
	distsSlice := (*[1 << 30]C.double)(unsafe.Pointer(res.distances))[:res.num_results:res.num_results]
	metasSlice := (*[1 << 30]*C.char)(unsafe.Pointer(res.metadatas_json))[:res.num_results:res.num_results]

	for i := 0; i < int(res.num_results); i++ {
		results[i] = QueryResult{
			ID:       C.GoString(idsSlice[i]),
			Distance: float64(distsSlice[i]),
			Metadata: C.GoString(metasSlice[i]),
		}
	}

	return results, nil
}

// Collection represents a document collection
type Collection struct {
	ptr *C.CCollection
}

// Collection retrieves a document collection
func (db *DBCenter) Collection(name string) *Collection {
	cName := C.CString(name)
	defer C.free(unsafe.Pointer(cName))

	res := C.dbcenter_get_collection(db.ptr, cName)

	if res.error != nil {
		msg := C.GoString(res.error)
		C.dbcenter_free_string(res.error) // Correctly free error string using generic free_string
		// Since we can't return error easily in this signature, and user didn't expect one,
		// we log and return nil or panic?
		// Panic is safer for now to avoid NPE later
		panic(fmt.Sprintf("Error getting collection '%s': %s", name, msg))
	}

	return &Collection{ptr: (*C.CCollection)(res.ptr)}
}

// Close closes the collection handle
func (c *Collection) Close() {
	if c.ptr != nil {
		C.dbcenter_collection_free(c.ptr)
		c.ptr = nil
	}
}

// InsertOne inserts a single document
func (c *Collection) InsertOne(doc interface{}) error {
	jsonBytes, err := json.Marshal(doc)
	if err != nil {
		return err
	}

	cJson := C.CString(string(jsonBytes))
	defer C.free(unsafe.Pointer(cJson))

	errPtr := C.dbcenter_collection_insert_one(c.ptr, cJson)
	if errPtr != nil {
		defer C.dbcenter_free_string(errPtr)
		return errors.New(C.GoString(errPtr))
	}
	return nil
}

func main() {
	fmt.Println("DBCenter Go wrapper initialized. Import this package to use.")
}
