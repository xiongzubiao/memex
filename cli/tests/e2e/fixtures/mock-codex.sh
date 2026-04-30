#!/usr/bin/env bash
# Mock `codex app-server --listen stdio://`.
#
# Mode selected via MOCK_CODEX_MODE:
#   ok (default) — canned synth-JSON answer with one citation
#   auth_fail    — turn/completed status=failed with error.kind=auth
#   crash        — exit 1 immediately (simulates subprocess crash before handshake)
#   timeout      — read but never respond (tests tokio::time::timeout)
#   agent_error  — turn/completed status=failed with no structured kind
#   non_json     — successful turn; assistant text is plain prose, not JSON
#
# Protocol: newline-delimited JSON-RPC 2.0. Requests on stdin;
# responses (with matching id) + notifications (no id) on stdout.

set -u

mode="${MOCK_CODEX_MODE:-ok}"

case "$mode" in
  crash) exit 1 ;;
  timeout)
    # Drain stdin to avoid SIGPIPE on the caller; never reply.
    while IFS= read -r _; do sleep 120; done
    ;;
esac

# Canned payloads. Answer JSON is double-escaped so it becomes a string
# inside the item/agentMessage/delta's `delta` field.
ok_answer='{\"answer\":\"Production rollout begins 2026-04-16 [[auth-migration-timeline]].\",\"citations\":[\"auth-migration-timeline\"]}'
nonjson_answer='I thought about this and the answer is probably 42.'

extract_id() {
  # Extract the integer id from a JSON-RPC request line.
  printf '%s' "$1" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'
}

extract_method() {
  # Extract the method string from a JSON-RPC request line.
  printf '%s' "$1" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p'
}

while IFS= read -r line; do
  id="$(extract_id "$line")"
  method="$(extract_method "$line")"

  case "$method" in
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"userAgent":"mock-codex"}}\n' "$id"
      ;;
    thread/start)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"thread":{"id":"mock-thread"}}}\n' "$id"
      ;;
    turn/start)
      # Ack the request first.
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"

      case "$mode" in
        auth_fail)
          printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"turn":{"status":"failed","error":{"kind":"auth","message":"unauthorized"}}}}\n'
          ;;
        agent_error)
          printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"turn":{"status":"failed","error":{"message":"overloaded"}}}}\n'
          ;;
        non_json)
          printf '{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"delta":"%s"}}\n' "$nonjson_answer"
          turn_total=$((${turn_total:-0} + 100))
          printf '{"jsonrpc":"2.0","method":"thread/tokenUsage/updated","params":{"total":{"inputTokens":%s}}}\n' "$turn_total"
          printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"turn":{"status":"completed"}}}\n'
          ;;
        ok|*)
          printf '{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"delta":"%s"}}\n' "$ok_answer"
          turn_total=$((${turn_total:-0} + 100))
          printf '{"jsonrpc":"2.0","method":"thread/tokenUsage/updated","params":{"total":{"inputTokens":%s}}}\n' "$turn_total"
          printf '{"jsonrpc":"2.0","method":"turn/completed","params":{"turn":{"status":"completed"}}}\n'
          ;;
      esac
      ;;
    *)
      # Unknown method: respond with generic ack so the worker doesn't hang.
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
  esac
done
