package com.dbcenter;

public class QueryResult {
    public String id;
    public double distance;
    public String metadata;

    public QueryResult(String id, double distance, String metadata) {
        this.id = id;
        this.distance = distance;
        this.metadata = metadata;
    }
    
    @Override
    public String toString() {
        return "QueryResult{id='" + id + "', distance=" + distance + ", metadata='" + metadata + "'}";
    }
}
