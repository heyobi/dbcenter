package com.dbcenter;

public class DBCenter implements AutoCloseable {
    private long ptr; // Pointer to Rust object

    // Load the native library
    static {
        System.loadLibrary("dbcenter_java");
    }

    // Native methods
    private native void nativeNew(String path, boolean persist);
    private native void nativeFree();
    private native void nativeInstallExtension(String name);
    private native VectorStore nativeGetVectorStore(String name, int dimension);

    public DBCenter(String path, boolean persist) {
        nativeNew(path, persist);
    }
    
    public DBCenter() {
        this(null, false);
    }

    @Override
    public void close() {
        nativeFree();
    }

    public void installExtension(String name) {
        nativeInstallExtension(name);
    }

    public VectorStore getVectorStore(String name, int dimension) {
        return nativeGetVectorStore(name, dimension);
    }
}
