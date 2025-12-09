package com.dbcenter;

import java.util.List;

public class VectorStore implements AutoCloseable {
    private long ptr;
    private int dimension;

    // Called from Rust
    private VectorStore(long ptr, int dimension) {
        this.ptr = ptr;
        this.dimension = dimension;
    }

    private native void nativeFree();
    private native void nativeAdd(String[] ids, float[] embeddings);
    private native QueryResult[] nativeQuery(float[] query, long nResults);

    @Override
    public void close() {
        nativeFree();
    }

    public void add(String[] ids, float[][] embeddings) {
        // Flatten embeddings for JNI
        int count = ids.length;
        if (embeddings.length != count) {
            throw new IllegalArgumentException("Ids and embeddings length mismatch");
        }
        
        float[] flat = new float[count * dimension];
        for (int i = 0; i < count; i++) {
             if (embeddings[i].length != dimension) throw new IllegalArgumentException("Dimension mismatch at index " + i);
             System.arraycopy(embeddings[i], 0, flat, i * dimension, dimension);
        }
        
        nativeAdd(ids, flat);
    }
    
    public QueryResult[] query(float[] query, long nResults) {
        if (query.length != dimension) throw new IllegalArgumentException("Query dimension mismatch");
        return nativeQuery(query, nResults);
    }
    
    public int getDimension() {
        return dimension;
    }
}
