#!/usr/bin/env bash
# Mock `gemini --acp`.
#
# Mode selected via MOCK_GEMINI_MODE:
#   ok (default) — canned synth-JSON answer with one citation
#   auth_fail    — session/prompt returns error with auth-related message
#   crash        — exit 1 immediately
#   timeout      — read but never respond
#   agent_error  — session/prompt returns stopReason != "end_turn"
#   non_json     — agent_message_chunk text is plain prose, not JSON
#
# Protocol: Zed ACP JSON-RPC 2.0 over stdio. Responses match id; notifications have no id.

set -u

mode="${MOCK_GEMINI_MODE:-ok}"

case "$mode" in
  crash) exit 1 ;;
  timeout)
    while IFS= read -r _; do sleep 120; done
    ;;
esac

ok_answer='{\"answer\":\"Production rollout begins 2026-04-16 [[auth-migration-timeline]].\",\"citations\":[\"auth-migration-timeline\"]}'
nonjson_answer='I thought about this and the answer is probably 42.'

extract_id() {
  printf '%s' "$1" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'
}

extract_method() {
  printf '%s' "$1" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p'
}

while IFS= read -r line; do
  id="$(extract_id "$line")"
  method="$(extract_method "$line")"

  case "$method" in
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"agentInfo":{"name":"mock-gemini-cli"},"agentCapabilities":{},"authMethods":[]}}\n' "$id"
      ;;
    session/new)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"mock-session","modes":{"currentModeId":"default"},"models":{"currentModelId":"gemini-3-flash-preview"}}}\n' "$id"
      ;;
    session/set_mode|session/set_model)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    session/prompt)
      case "$mode" in
        auth_fail)
          printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"authentication required"}}\n' "$id"
          ;;
        agent_error)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"max_tokens"}}\n' "$id"
          ;;
        non_json)
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"%s"}}}}\n' "$nonjson_answer"
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","_meta":{"quota":{"token_count":{"input_tokens":100}}}}}\n' "$id"
          ;;
        ok|*)
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"%s"}}}}\n' "$ok_answer"
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","_meta":{"quota":{"token_count":{"input_tokens":100}}}}}\n' "$id"
          ;;
      esac
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
  esac
done
