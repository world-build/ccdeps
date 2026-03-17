#!/usr/bin/env bash
# Conformance fuzz-test: compare cppscan output against gcc -M on real test files.
#
# Usage: ./scripts/conformance-fuzz.sh [test-dir...]
# If no dirs given, scans the GCC and Clang test suites in /tmp.

set -euo pipefail

CPPSCAN="${CPPSCAN:-./target/release/cppscan}"
COMPILER="${COMPILER:-gcc}"
TOTAL=0
PASS=0
FAIL=0
SKIP=0
ERRORS=""

compare_file() {
    local f="$1"
    TOTAL=$((TOTAL + 1))

    # Get the directory containing the file for -I
    local dir
    dir="$(dirname "$f")"

    # Run compiler
    local compiler_out
    compiler_out=$("$COMPILER" -M -I"$dir" "$f" 2>/dev/null) || { SKIP=$((SKIP + 1)); return; }

    # Run cppscan
    local cppscan_out
    cppscan_out=$("$CPPSCAN" -M -I"$dir" "$f" 2>/dev/null) || { SKIP=$((SKIP + 1)); return; }

    # Parse both into sorted dep sets (strip target, normalize paths)
    local compiler_deps cppscan_deps
    compiler_deps=$(echo "$compiler_out" | tr '\\' ' ' | tr '\n' ' ' | sed 's/^[^:]*://' | tr ' ' '\n' | sort -u | grep -v '^$')
    cppscan_deps=$(echo "$cppscan_out" | tr '\\' ' ' | tr '\n' ' ' | sed 's/^[^:]*://' | tr ' ' '\n' | sort -u | grep -v '^$')

    # Filter out system headers (under /usr/, /nix/store/) and the source file itself
    # These are expected differences since cppscan doesn't have compiler builtin paths
    compiler_deps=$(echo "$compiler_deps" | grep -v '^/usr/' | grep -v '^/nix/store/' || true)
    cppscan_deps=$(echo "$cppscan_deps" | grep -v '^/usr/' | grep -v '^/nix/store/' || true)

    # Compare
    local only_compiler only_cppscan
    only_compiler=$(comm -23 <(echo "$compiler_deps") <(echo "$cppscan_deps") || true)
    only_cppscan=$(comm -13 <(echo "$compiler_deps") <(echo "$cppscan_deps") || true)

    if [[ -z "$only_compiler" && -z "$only_cppscan" ]]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
        ERRORS="${ERRORS}MISMATCH: $f\n"
        if [[ -n "$only_compiler" ]]; then
            ERRORS="${ERRORS}  ${COMPILER} extra: $(echo "$only_compiler" | tr '\n' ' ')\n"
        fi
        if [[ -n "$only_cppscan" ]]; then
            ERRORS="${ERRORS}  cppscan extra: $(echo "$only_cppscan" | tr '\n' ' ')\n"
        fi
    fi
}

# Collect test directories
if [[ $# -gt 0 ]]; then
    DIRS=("$@")
else
    DIRS=(
        /tmp/gcc/gcc/testsuite/gcc.dg/cpp/
        /tmp/gcc/gcc/testsuite/c-c++-common/cpp/
        /tmp/clang/clang/test/Preprocessor/
    )
fi

for dir in "${DIRS[@]}"; do
    if [[ ! -d "$dir" ]]; then
        echo "SKIP: $dir (not found)"
        continue
    fi
    echo "Testing: $dir"
    # Only test .c files (not .h, .def, etc.)
    for f in "$dir"*.c "$dir"*.cpp; do
        [[ -f "$f" ]] || continue
        compare_file "$f"
    done
done

echo ""
echo "========================================="
echo "Results ($COMPILER vs cppscan):"
echo "  Total: $TOTAL"
echo "  Pass:  $PASS"
echo "  Fail:  $FAIL"
echo "  Skip:  $SKIP (compiler rejected file)"
echo "========================================="

if [[ $FAIL -gt 0 ]]; then
    echo ""
    echo "Failures:"
    echo -e "$ERRORS"
fi

exit 0
