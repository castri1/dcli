#!/bin/sh
# Try the UI without AWS: starts a local fake AWS and opens dcli against it.
# Usage: dev/demo.sh [dcli args]   e.g. dev/demo.sh status dev app-dev
set -e
cd "$(dirname "$0")"
PORT=${PORT:-47123}
python3 mock_aws.py "$PORT" &
MOCK=$!
trap 'kill $MOCK 2>/dev/null' EXIT INT TERM
sleep 0.5
export AWS_ENDPOINT_URL="http://127.0.0.1:$PORT"
export AWS_ACCESS_KEY_ID=AKIDTEST AWS_SECRET_ACCESS_KEY=test
export AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null
cargo run --release --quiet -- --config demo-config.yaml "$@"
