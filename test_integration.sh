#!/usr/bin/env bash
set -e

TRACKER=./target/debug/dragonfly-tracker
PORT=18081

$TRACKER --bind "127.0.0.1:$PORT" &
TRACKER_PID=$!
trap "kill $TRACKER_PID 2>/dev/null; wait $TRACKER_PID 2>/dev/null" EXIT
sleep 1

echo "=== Tracker running (pid $TRACKER_PID) ==="

CONTENT_KEY=$(printf 'hf://owner/repo/model.gguf:main' | sha256sum | awk '{print $1}')
echo "=== Content key: $CONTENT_KEY ==="

NODE_ID="abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"

echo "=== Announcing peer ==="
ANNOUNCE_RESP=$(curl -sf -X POST "http://127.0.0.1:$PORT/announce" \
  -H 'Content-Type: application/json' \
  -d "{\"content_key\":\"$CONTENT_KEY\",\"node_id\":\"$NODE_ID\",\"addr_info\":\"{}\"}")
echo "Announce response: $ANNOUNCE_RESP"

echo "=== Querying peers ==="
PEERS=$(curl -sf "http://127.0.0.1:$PORT/peers?content_key=$CONTENT_KEY")
echo "Peers response: $PEERS"
PEER_COUNT=$(echo "$PEERS" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["providers"]))')
echo "Provider count: $PEER_COUNT"

echo "=== Announcing peer with content metadata ==="
META_KEY=$(printf 'hf://owner/repo/model.safetensors:main' | sha256sum | awk '{print $1}')
curl -sf -X POST "http://127.0.0.1:$PORT/announce" \
  -H 'Content-Type: application/json' \
  -d "{\"content_key\":\"$META_KEY\",\"node_id\":\"$NODE_ID\",\"addr_info\":\"{}\",\"filename\":\"model.safetensors\",\"format\":\"safetensors\",\"size\":12345}" \
  > /dev/null

echo "=== Querying contents (category filter + search) ==="
CONTENTS=$(curl -sf "http://127.0.0.1:$PORT/contents?format=safetensors&q=model")
echo "Contents response: $CONTENTS"
CONTENT_COUNT=$(echo "$CONTENTS" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["contents"]))')
CONTENT_NAME=$(echo "$CONTENTS" | python3 -c 'import sys,json; c=json.load(sys.stdin)["contents"]; print(c[0]["filename"] if c else "")')
echo "Filtered content count: $CONTENT_COUNT (filename: $CONTENT_NAME)"

echo "=== Registry UI page ==="
UI_OK=0
if curl -sf "http://127.0.0.1:$PORT/?format=safetensors" | grep -q "model.safetensors"; then
  UI_OK=1
  echo "UI page lists the announced file"
fi

echo "=== Rate limit test (11 announces from same IP) ==="
RATE_BLOCKED=0
for i in $(seq 1 11); do
  CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST "http://127.0.0.1:$PORT/announce" \
    -H 'Content-Type: application/json' \
    -d "{\"content_key\":\"$CONTENT_KEY\",\"node_id\":\"$NODE_ID\",\"addr_info\":\"{}\"}")
  if [ "$CODE" = "429" ]; then
    RATE_BLOCKED=1
    echo "Got 429 on attempt $i (rate limit working)"
    break
  fi
done

echo "=== Testing leave ==="
curl -sf -X DELETE "http://127.0.0.1:$PORT/leave" \
  -H 'Content-Type: application/json' \
  -d "{\"content_key\":\"$CONTENT_KEY\",\"node_id\":\"$NODE_ID\"}"
echo

PEERS_AFTER=$(curl -sf "http://127.0.0.1:$PORT/peers?content_key=$CONTENT_KEY")
PEER_COUNT_AFTER=$(echo "$PEERS_AFTER" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["providers"]))')
echo "Provider count after leave: $PEER_COUNT_AFTER"

echo ""
if [ "$PEER_COUNT" = "1" ] && [ "$PEER_COUNT_AFTER" = "0" ] && [ "$RATE_BLOCKED" = "1" ] \
  && [ "$CONTENT_COUNT" = "1" ] && [ "$CONTENT_NAME" = "model.safetensors" ] && [ "$UI_OK" = "1" ]; then
  echo "=== ALL TRACKER TESTS PASSED ==="
else
  echo "=== TRACKER TESTS FAILED (peers=$PEER_COUNT, after_leave=$PEER_COUNT_AFTER, rate_blocked=$RATE_BLOCKED, contents=$CONTENT_COUNT/$CONTENT_NAME, ui=$UI_OK) ==="
  exit 1
fi
