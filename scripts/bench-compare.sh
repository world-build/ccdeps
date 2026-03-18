#!/usr/bin/env bash
# Wall-clock comparison: cppscan vs gcc/clang/clang-scan-deps on real C++ source files.
#
# Usage: ./scripts/bench-compare.sh [clang-lib-dir]
# Default: uses /tmp/clang/clang/lib as the source tree.
#
# Requires: gcc, clang, clang-scan-deps, target/release/cppscan

set -eu

CPPSCAN="${CPPSCAN:-./target/release/cppscan}"
CLANG_LIB="${1:-/tmp/clang/clang/lib}"
CLANG_INC="/tmp/clang/clang/include"
NPROC=$(nproc)
RUNS=5

if [[ ! -d "$CLANG_LIB" ]]; then
    echo "Error: $CLANG_LIB not found"
    exit 1
fi
if [[ ! -x "$CPPSCAN" ]]; then
    echo "Building cppscan in release mode..."
    cargo build --release
fi

# Check available tools
HAS_GCC=$(command -v gcc >/dev/null 2>&1 && echo 1 || echo 0)
HAS_CLANG=$(command -v clang >/dev/null 2>&1 && echo 1 || echo 0)
HAS_CLANG_SCAN_DEPS=$(command -v clang-scan-deps >/dev/null 2>&1 && echo 1 || echo 0)

# Collect all .cpp files
FILE_LIST=$(mktemp)
find "$CLANG_LIB" -name '*.cpp' -type f | sort > "$FILE_LIST"
FILE_COUNT=$(wc -l < "$FILE_LIST")

echo "Source tree: $CLANG_LIB ($FILE_COUNT .cpp files)"
echo "Include path: $CLANG_INC"
echo "Parallel jobs: $NPROC"
echo "Runs per benchmark: $RUNS"
echo "Tools: gcc=$([ "$HAS_GCC" = 1 ] && gcc --version 2>/dev/null | head -1 || echo 'n/a')"
echo "       clang=$([ "$HAS_CLANG" = 1 ] && clang --version 2>/dev/null | head -1 || echo 'n/a')"
echo "       clang-scan-deps=$([ "$HAS_CLANG_SCAN_DEPS" = 1 ] && echo 'yes' || echo 'n/a')"
echo ""

# ---------------------------------------------------------------------------
# Helper: run N times using date +%s%N for timing, report min/median/mean
# ---------------------------------------------------------------------------
run_timed() {
    local label="$1"
    shift
    local times=()
    for _ in $(seq 1 $RUNS); do
        local start_ns end_ns elapsed_ms
        start_ns=$(date +%s%N)
        eval "$@" > /dev/null 2>&1 || true
        end_ns=$(date +%s%N)
        elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))
        times+=("$elapsed_ms")
    done

    # Sort and compute stats
    local sorted
    sorted=$(printf '%s\n' "${times[@]}" | sort -n)
    local min_t median_t mean_t
    min_t=$(echo "$sorted" | head -1)
    median_t=$(echo "$sorted" | sed -n "$((($RUNS + 1) / 2))p")
    mean_t=$(printf '%s\n' "${times[@]}" | awk '{s+=$1} END {printf "%.0f", s/NR}')

    printf "  %-45s  min=%5dms  median=%5dms  mean=%5dms\n" "$label" "$min_t" "$median_t" "$mean_t"
}

# ---------------------------------------------------------------------------
# Generate compile_commands.json for clang-scan-deps
# ---------------------------------------------------------------------------
generate_compile_commands() {
    local file_list="$1"
    local output="$2"
    echo "[" > "$output"
    local first=1
    while IFS= read -r f; do
        if [[ $first -eq 0 ]]; then
            echo "," >> "$output"
        fi
        first=0
        printf '{"directory":"/tmp","command":"clang -M -MG -I%s %s","file":"%s"}' \
            "$CLANG_INC" "$f" "$f" >> "$output"
    done < "$file_list"
    echo "]" >> "$output"
}

# ---------------------------------------------------------------------------
# Single-file benchmark
# ---------------------------------------------------------------------------
SINGLE_FILE="$CLANG_LIB/Sema/SemaExpr.cpp"
if [[ ! -f "$SINGLE_FILE" ]]; then
    SINGLE_FILE=$(head -1 "$FILE_LIST")
fi
SINGLE_SIZE=$(wc -c < "$SINGLE_FILE")
SINGLE_NAME=$(basename "$SINGLE_FILE")

echo "=== Single file: $SINGLE_NAME ($(( SINGLE_SIZE / 1024 ))KB) ==="
echo ""

if [[ "$HAS_GCC" = 1 ]]; then
    run_timed "gcc -M" "gcc -M -MG -I '$CLANG_INC' '$SINGLE_FILE'"
fi

if [[ "$HAS_CLANG" = 1 ]]; then
    run_timed "clang -M" "clang -M -MG -I '$CLANG_INC' '$SINGLE_FILE'"
fi

if [[ "$HAS_CLANG_SCAN_DEPS" = 1 ]]; then
    SINGLE_COMPDB=$(mktemp --suffix=.json)
    echo "$SINGLE_FILE" > /tmp/_single_file.txt
    generate_compile_commands /tmp/_single_file.txt "$SINGLE_COMPDB"
    run_timed "clang-scan-deps (1 file)" "clang-scan-deps -compilation-database '$SINGLE_COMPDB' -format make -j 1"
    rm -f "$SINGLE_COMPDB" /tmp/_single_file.txt
fi

run_timed "cppscan" "'$CPPSCAN' -M --MG -I '$CLANG_INC' '$SINGLE_FILE'"
echo ""

# ---------------------------------------------------------------------------
# Multi-file benchmark
# ---------------------------------------------------------------------------
echo "=== All $FILE_COUNT files ==="
echo ""

if [[ "$HAS_GCC" = 1 ]]; then
    run_timed "gcc -M (sequential)" \
        "while IFS= read -r f; do gcc -M -MG -I '$CLANG_INC' \"\$f\" 2>/dev/null; done < '$FILE_LIST'"

    run_timed "gcc -M (parallel, ${NPROC} jobs)" \
        "cat '$FILE_LIST' | xargs -P $NPROC -I{} gcc -M -MG -I '$CLANG_INC' {} 2>/dev/null"
fi

if [[ "$HAS_CLANG" = 1 ]]; then
    run_timed "clang -M (sequential)" \
        "while IFS= read -r f; do clang -M -MG -I '$CLANG_INC' \"\$f\" 2>/dev/null; done < '$FILE_LIST'"

    run_timed "clang -M (parallel, ${NPROC} jobs)" \
        "cat '$FILE_LIST' | xargs -P $NPROC -I{} clang -M -MG -I '$CLANG_INC' {} 2>/dev/null"
fi

if [[ "$HAS_CLANG_SCAN_DEPS" = 1 ]]; then
    FULL_COMPDB=$(mktemp --suffix=.json)
    generate_compile_commands "$FILE_LIST" "$FULL_COMPDB"

    run_timed "clang-scan-deps (${FILE_COUNT} files, j=1)" \
        "clang-scan-deps -compilation-database '$FULL_COMPDB' -format make -j 1"

    run_timed "clang-scan-deps (${FILE_COUNT} files, j=${NPROC})" \
        "clang-scan-deps -compilation-database '$FULL_COMPDB' -format make -j $NPROC"

    rm -f "$FULL_COMPDB"
fi

# cppscan (single invocation, internally parallel via @file)
RESPONSE_FILE=$(mktemp)
cp "$FILE_LIST" "$RESPONSE_FILE"
run_timed "cppscan (all files, parallel)" "'$CPPSCAN' -M --MG -I '$CLANG_INC' '@$RESPONSE_FILE'"

echo ""

# ---------------------------------------------------------------------------
# Scaling
# ---------------------------------------------------------------------------
echo "=== Scaling: cppscan file count ==="
echo ""
for n in 1 10 100 500 "$FILE_COUNT"; do
    n=$((n < FILE_COUNT ? n : FILE_COUNT))
    SUBSET=$(mktemp)
    head -n "$n" "$FILE_LIST" > "$SUBSET"
    run_timed "cppscan ($n files)" "'$CPPSCAN' -M --MG -I '$CLANG_INC' '@$SUBSET'"
    rm -f "$SUBSET"
done

echo ""
echo "=== Done ==="

rm -f "$FILE_LIST" "$RESPONSE_FILE"
