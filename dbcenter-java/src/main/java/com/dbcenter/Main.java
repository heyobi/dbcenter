package com.dbcenter;

public class Main {
    public static void main(String[] args) {
        System.out.println("Starting Java test...");
        try (DBCenter db = new DBCenter("java_test_db", false)) {
            System.out.println("DBCenter created.");
            try (VectorStore vs = db.getVectorStore("java_vecs", 3)) {
                System.out.println("VectorStore created.");

                String[] ids = { "j1", "j2" };
                float[][] embeddings = {
                        { 0.1f, 0.2f, 0.3f },
                        { 0.4f, 0.5f, 0.6f }
                };

                vs.add(ids, embeddings);
                System.out.println("Vectors added.");

                float[] query = { 0.1f, 0.2f, 0.3f };
                QueryResult[] results = vs.query(query, 5);
                System.out.println("Query returned " + results.length + " results.");
                for (QueryResult res : results) {
                    System.out.println(res);
                }
            }
        } catch (Exception e) {
            e.printStackTrace();
            System.exit(1);
        }
        System.out.println("Java Done.");
    }
}
