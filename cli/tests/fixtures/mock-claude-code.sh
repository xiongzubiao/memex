#!/usr/bin/env bash
# Mock `claude -p --input-format stream-json --output-format stream-json`.
#
# Mode selected via MOCK_CLAUDE_CODE_MODE:
#   ok (default) — canned JSON answer with one citation
#   auth_fail    — structured auth error via result.is_error + assistant error
#   crash        — exit 1 immediately, no output
#   timeout      — sleep 120s, then exit (far beyond default 60s timeout)
#   agent_error  — result.is_error=true with api_error_status="overloaded"
#   non_json     — valid stream-json but assistant text isn't our JSON shape
#   expand_then_synth_ok — turn 1 emits expansion JSON, turn 2+ emits synth JSON
#
# The integration test symlinks this file as `claude` and prepends its
# directory to PATH so the daemon's worker launches this instead of the
# real binary.

set -u

mode="${MOCK_CLAUDE_CODE_MODE:-ok}"

case "$mode" in
  crash)
    # Exit immediately with no output. Worker sees EOF before result.
    exit 1
    ;;

  timeout)
    # Read but never reply. Worker's tokio::time::timeout fires.
    while IFS= read -r _; do
      sleep 120
    done
    ;;
esac

# For all other modes, emit init + loop over user messages.
printf '{"type":"system","subtype":"init"}\n'

turn=0
while IFS= read -r _line; do
  turn=$((turn + 1))
  case "$mode" in
    auth_fail)
      printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Not logged in · Please run /login"}]},"error":"authentication_failed"}\n'
      printf '{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}\n'
      ;;
    agent_error)
      printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Overloaded — try again later."}]}}\n'
      printf '{"type":"result","subtype":"success","is_error":true,"api_error_status":"overloaded","result":"Overloaded — try again later."}\n'
      ;;
    non_json)
      printf '{"type":"assistant","message":{"content":[{"type":"text","text":"I thought about this and the answer is probably 42."}]}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"result":"","usage":{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}\n'
      ;;
    expand_then_synth_ok)
      if [ "$turn" -eq 1 ]; then
        # Turn 1: expansion JSON {lex, vec, hyde}
        printf '{"type":"assistant","message":{"content":[{"type":"text","text":"{\\"lex\\":\\"rollout\\",\\"vec\\":\\"when did the production release happen\\",\\"hyde\\":\\"Production rollout began on 2026-04-16.\\"}"}]}}\n'
        printf '{"type":"result","subtype":"success","is_error":false,"result":"","usage":{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}\n'
      else
        # Turn 2+: synth JSON {answer, citations}
        printf '{"type":"assistant","message":{"content":[{"type":"text","text":"{\\"answer\\":\\"Production rollout began 2026-04-16 [[auth-migration-timeline]].\\",\\"citations\\":[\\"auth-migration-timeline\\"]}"}]}}\n'
        printf '{"type":"result","subtype":"success","is_error":false,"result":"","usage":{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}\n'
      fi
      ;;
    ok|*)
      printf '{"type":"assistant","message":{"content":[{"type":"text","text":"{\\"answer\\":\\"Production rollout begins 2026-04-16 [[auth-migration-timeline]].\\",\\"citations\\":[\\"auth-migration-timeline\\"]}"}]}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"result":"","usage":{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}\n'
      ;;
  esac
done
