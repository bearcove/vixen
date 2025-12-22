#!/bin/bash
pkill -f vx-casd || true
sleep 0.3

RUST_LOG=trace ./target/debug/vx-casd 2>&1 &
SERVER_PID=$!
echo "Server PID: $SERVER_PID"
sleep 0.5

RUST_LOG=trace timeout 2 ./target/debug/examples/test_client 2>&1

kill $SERVER_PID 2>/dev/null || true
pkill -f vx-casd || true
