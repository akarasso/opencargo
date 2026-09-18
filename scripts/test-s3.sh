#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

image=${MINIO_IMAGE:-quay.io/minio/minio:RELEASE.2025-09-07T16-13-09Z}
port=${MINIO_PORT:-19000}
bucket=${OPENCARGO_TEST_S3_BUCKET:-opencargo-test}
name=opencargo-test-minio-$$
user=minio
pass=minio12345

docker run -d --rm --name "$name" -p "127.0.0.1:$port:9000" \
  -e MINIO_ROOT_USER=$user -e MINIO_ROOT_PASSWORD=$pass "$image" server /data >/dev/null
trap 'docker stop "$name" >/dev/null' EXIT

deadline=$((SECONDS + 60))
until curl -sf "http://127.0.0.1:$port/minio/health/live" >/dev/null; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "minio did not come up" >&2
    exit 1
  fi
  sleep 1
done
curl -sf -X PUT --aws-sigv4 "aws:amz:us-east-1:s3" --user "$user:$pass" "http://127.0.0.1:$port/$bucket" >/dev/null

export OPENCARGO_TEST_STORAGE=s3
export OPENCARGO_TEST_S3_BUCKET=$bucket
export OPENCARGO_S3_ENDPOINT=http://127.0.0.1:$port
export OPENCARGO_S3_ACCESS_KEY_ID=$user
export OPENCARGO_S3_SECRET_ACCESS_KEY=$pass
cargo test --workspace --no-fail-fast "$@"
