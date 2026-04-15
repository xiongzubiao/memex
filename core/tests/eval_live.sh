#!/usr/bin/env bash
#
# Live full-pipeline evaluation using the real memex CLI.
# Self-contained: creates a temp memex, ingests eval docs, runs queries.
#
# Usage:
#   bash core/tests/eval_live.sh [memex-binary]
#
# Equivalent of qmd's eval-harness.ts — runs against a real LLM provider,
# measures true end-to-end retrieval quality.

set -euo pipefail

MEMEX="${1:-target/debug/memex}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
EVAL_DOCS="$SCRIPT_DIR/eval-docs"

if [ ! -f "$MEMEX" ]; then
    echo "Binary not found: $MEMEX"
    echo "Run: cargo build"
    exit 1
fi

if [ ! -d "$EVAL_DOCS" ]; then
    echo "Eval docs not found: $EVAL_DOCS"
    exit 1
fi

# Create a temp memex instance
EVAL_ROOT=$(mktemp -d /tmp/memex-eval-XXXXXX)
export MEMEX_ROOT="$EVAL_ROOT"
trap 'rm -rf "$EVAL_ROOT"' EXIT

echo "Setting up eval memex at $EVAL_ROOT ..."
"$MEMEX" init 2>/dev/null

# Write eval docs as wiki pages with frontmatter (no LLM needed)
mkdir -p "$EVAL_ROOT/wiki"
for f in "$EVAL_DOCS"/*.md; do
    filename=$(basename "$f")
    stem="${filename%.md}"
    title=$(head -1 "$f" | sed 's/^# //')
    tags=$(echo "$stem" | tr '-' ' ')
    cat > "$EVAL_ROOT/wiki/$filename" << ENDPAGE
---
title: $title
tags:
  - $tags
created: 2026-01-01T00:00:00Z
last_updated: 2026-01-01T00:00:00Z
sources: []
---

$(cat "$f")
ENDPAGE
done

# Rebuild index + search DB
"$MEMEX" wiki reindex 2>/dev/null
echo "Setup complete: $(ls "$EVAL_ROOT/wiki/"*.md | wc -l | tr -d ' ') docs ingested"
echo ""

# Queries: same as qmd's eval-bm25.test.ts
# Format: difficulty|query|expected_doc_substring
QUERIES=(
    # EASY
    "easy|API versioning|api-design"
    "easy|Series A fundraising|fundraising"
    "easy|CAP theorem|distributed-systems"
    "easy|overfitting machine learning|machine-learning"
    "easy|remote work VPN|remote-work"
    "easy|Project Phoenix retrospective|product-launch"
    # MEDIUM
    "medium|how to structure REST endpoints|api-design"
    "medium|raising money for startup|fundraising"
    "medium|consistency vs availability tradeoffs|distributed-systems"
    "medium|how to prevent models from memorizing data|machine-learning"
    "medium|working from home guidelines|remote-work"
    "medium|what went wrong with the launch|product-launch"
    # HARD
    "hard|nouns not verbs|api-design"
    "hard|Sequoia investor pitch|fundraising"
    "hard|Raft algorithm leader election|distributed-systems"
    "hard|F1 score precision recall|machine-learning"
    "hard|quarterly team gathering travel|remote-work"
    "hard|beta program 47 bugs|product-launch"
)

echo "=== Memex Live Eval ($(date)) ==="
echo "Binary: $MEMEX"
echo "Root:   $EVAL_ROOT"
echo "Queries: ${#QUERIES[@]}"
echo ""

# =========================================================================
# Shared eval function — runs queries, accumulates per-tier hit counts.
#
# Usage: run_eval <label> <cmd_template> <extract_top>
#   label:        section name for display
#   cmd_template: command with %QUERY% placeholder
#   extract_top:  command to extract the top result line from output
#
# Sets global variables: _easy_hit, _easy_total, _medium_hit, _medium_total,
#   _hard_hit, _hard_total, _easy_pct, _medium_pct, _hard_pct, _overall_pct
# =========================================================================
run_eval() {
    local label="$1" cmd_template="$2" extract_top="$3"

    echo "--- $label ---"
    echo ""

    _easy_total=0; _easy_hit=0
    _medium_total=0; _medium_hit=0
    _hard_total=0; _hard_hit=0

    for entry in "${QUERIES[@]}"; do
        IFS='|' read -r difficulty query expected <<< "$entry"

        local cmd="${cmd_template//%QUERY%/$query}"
        local output
        output=$(eval "$cmd" 2>/dev/null || echo "")

        local status="✗"
        if echo "$output" | grep -qi "$expected"; then
            status="✓"
            case "$difficulty" in
                easy)   _easy_hit=$((_easy_hit + 1)) ;;
                medium) _medium_hit=$((_medium_hit + 1)) ;;
                hard)   _hard_hit=$((_hard_hit + 1)) ;;
            esac
        fi

        case "$difficulty" in
            easy)   _easy_total=$((_easy_total + 1)) ;;
            medium) _medium_total=$((_medium_total + 1)) ;;
            hard)   _hard_total=$((_hard_total + 1)) ;;
        esac

        local top
        top=$(echo "$output" | eval "$extract_top" || echo "none")
        printf "[%-6s] %s %-50s → %s (%s)\n" "$difficulty" "$status" "$query" "$expected" "$top"
    done

    _easy_pct=$((_easy_hit * 100 / _easy_total))
    _medium_pct=$((_medium_hit * 100 / _medium_total))
    _hard_pct=$((_hard_hit * 100 / _hard_total))
    local _total=$((_easy_total + _medium_total + _hard_total))
    local _total_hit=$((_easy_hit + _medium_hit + _hard_hit))
    _overall_pct=$((_total_hit * 100 / _total))

    echo ""
}

# =========================================================================
# 1. BM25-Only Search (memex wiki search, no LLM)
# =========================================================================
run_eval \
    "BM25-Only Search (memex wiki search, no LLM)" \
    "\"$MEMEX\" wiki search \"%QUERY%\" -k 5" \
    "head -1"

bm25_easy_pct=$_easy_pct
bm25_medium_pct=$_medium_pct
bm25_hard_pct=$_hard_pct
bm25_overall_pct=$_overall_pct

# =========================================================================
# 2. Full Pipeline (BM25 + LLM expansion + RRF fusion)
# =========================================================================
run_eval \
    "Full Pipeline (memex query, BM25 + LLM expansion + RRF)" \
    "\"$MEMEX\" query \"%QUERY%\"" \
    "grep -m1 '  - ' | sed 's/^  - //'"

pipe_easy_pct=$_easy_pct
pipe_medium_pct=$_medium_pct
pipe_hard_pct=$_hard_pct
pipe_overall_pct=$_overall_pct

# =========================================================================
# 3. Summary
# =========================================================================
echo "--- Summary ---"
echo ""

printf "%-10s %10s %10s\n" "Tier" "BM25" "Pipeline"
printf "%-10s %10s %10s\n" "--------" "--------" "--------"
printf "%-10s %9d%% %9d%%\n" "Easy"    $bm25_easy_pct    $pipe_easy_pct
printf "%-10s %9d%% %9d%%\n" "Medium"  $bm25_medium_pct  $pipe_medium_pct
printf "%-10s %9d%% %9d%%\n" "Hard"    $bm25_hard_pct    $pipe_hard_pct
printf "%-10s %9d%% %9d%%\n" "Overall" $bm25_overall_pct $pipe_overall_pct

# QMD BM25 thresholds (from qmd's eval-bm25.test.ts):
#   Easy ≥80%, Medium ≥15%, Hard ≥15%, Overall ≥40%

echo ""
echo "--- vs QMD Thresholds (eval-bm25.test.ts) ---"
echo ""
printf "%-10s %10s %10s %10s %10s\n" "Tier" "BM25" "Pipeline" "QMD" "Status"
printf "%-10s %10s %10s %10s %10s\n" "--------" "--------" "--------" "--------" "--------"

check_threshold() {
    local label=$1 bm25=$2 pipeline=$3 threshold=$4
    local status="PASS"
    [ "$bm25" -lt "$threshold" ] && status="FAIL"
    printf "%-10s %9d%% %9d%% %9d%% %10s\n" "$label" "$bm25" "$pipeline" "$threshold" "$status"
}

check_threshold "Easy"    $bm25_easy_pct    $pipe_easy_pct    80
check_threshold "Medium"  $bm25_medium_pct  $pipe_medium_pct  15
check_threshold "Hard"    $bm25_hard_pct    $pipe_hard_pct    15
check_threshold "Overall" $bm25_overall_pct $pipe_overall_pct 40

echo ""
echo "Note: Status is PASS/FAIL for BM25-only vs QMD thresholds."
echo "Pipeline should always exceed BM25 (adds LLM expansion + RRF fusion)."

# --- BM25 Search Latency ---
echo ""
echo "--- BM25 Search Latency ---"
echo ""

now_ns() { perl -MTime::HiRes -e 'printf "%d\n", Time::HiRes::time()*1e9'; }

latency_queries=("kubernetes deployment" "database performance" "authentication security")
total_ms=0
count=0
for lq in "${latency_queries[@]}"; do
    start_ns=$(now_ns)
    "$MEMEX" wiki search "$lq" -k 10 >/dev/null 2>&1
    end_ns=$(now_ns)
    elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))
    total_ms=$((total_ms + elapsed_ms))
    count=$((count + 1))
    printf "  %-40s %dms\n" "$lq" "$elapsed_ms"
done
avg_ms=$((total_ms / count))
printf "  %-40s %dms\n" "Average" "$avg_ms"

# --- Index Size ---
echo ""
echo "--- Index Size ---"
echo ""
db_file="$EVAL_ROOT/.search.db"
if [ -f "$db_file" ]; then
    db_kb=$(du -k "$db_file" | cut -f1)
    wiki_kb=$(du -sk "$EVAL_ROOT/wiki" | cut -f1)
    printf "  Wiki content:  %d KB\n" "$wiki_kb"
    printf "  Search index:  %d KB\n" "$db_kb"
    if [ "$wiki_kb" -gt 0 ]; then
        ratio=$(echo "scale=1; $db_kb / $wiki_kb" | bc 2>/dev/null || echo "n/a")
        printf "  Ratio:         %sx\n" "$ratio"
    fi
fi
