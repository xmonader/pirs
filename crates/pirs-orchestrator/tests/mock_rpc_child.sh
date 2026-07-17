#!/bin/bash
while IFS= read -r line; do
  parsed=$(python3 -c "import json,sys; d=json.loads(sys.stdin.read()); print(d.get('type',''), d.get('id','') or '')" <<< "$line")
  ty=$(echo "$parsed" | cut -d' ' -f1); id=$(echo "$parsed" | cut -d' ' -f2)
  case "$ty" in
    get_state)
      echo "{\"id\":\"$id\",\"type\":\"response\",\"command\":\"get_state\",\"success\":true,\"data\":{\"sessionId\":\"${PIRS_TEST_VAR:-fake-sid-123}\",\"sessionFile\":\"/tmp/fake.jsonl\",\"model\":{\"id\":\"fake-model\",\"provider\":\"openai\"},\"isStreaming\":false}}" ;;
    get_messages)
      echo "{\"id\":\"$id\",\"type\":\"response\",\"command\":\"get_messages\",\"success\":true,\"data\":{\"messages\":[]}}" ;;
    *)
      echo "{\"id\":\"$id\",\"type\":\"response\",\"command\":\"$ty\",\"success\":true}" ;;
  esac
done
