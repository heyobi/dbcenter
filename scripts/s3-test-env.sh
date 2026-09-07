#!/bin/sh
# Point the S3 integration tests at a local MinIO.
#
#   docker run -d --name dbcenter-minio -p 19000:9000 \
#     -e MINIO_ROOT_USER=dbcenter -e MINIO_ROOT_PASSWORD=dbcenter123 \
#     minio/minio:latest server /data
#   docker exec dbcenter-minio mc alias set local http://127.0.0.1:9000 dbcenter dbcenter123
#   docker exec dbcenter-minio mc mb local/dbcenter-test
#
# Then:  . scripts/s3-test-env.sh && cargo test -p dbcenter-core --test s3_storage
export DBCENTER_S3_TEST_ENDPOINT=http://127.0.0.1:19000
export DBCENTER_S3_TEST_BUCKET=dbcenter-test
export DBCENTER_S3_TEST_KEY=dbcenter
export DBCENTER_S3_TEST_SECRET=dbcenter123
