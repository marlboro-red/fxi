#!/bin/bash
# Benchmark script for fxi vs ripgrep vs grep
# Run from the root of a codebase (e.g., Linux kernel)
# Outputs results to benchmark_results.md

set -e

OUTPUT="benchmark_results.md"
RUNS=3  # Number of runs per benchmark for averaging

# Colors for terminal output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}=== fxi Benchmark Suite ===${NC}"
echo ""

# Check dependencies
command -v fxi >/dev/null 2>&1 || { echo -e "${RED}fxi not found in PATH${NC}"; exit 1; }
command -v rg >/dev/null 2>&1 || { echo -e "${RED}ripgrep (rg) not found${NC}"; exit 1; }

# Get codebase info
CODEBASE_NAME=$(basename "$(pwd)")
FILE_COUNT=$(find . -type f 2>/dev/null | wc -l | tr -d ' ')

echo -e "Codebase: ${YELLOW}$CODEBASE_NAME${NC}"
echo -e "Total files: ${YELLOW}$FILE_COUNT${NC}"
echo ""

# Check if fxi index exists, build if not
echo -e "${GREEN}Checking fxi index...${NC}"
if ! fxi stats . >/dev/null 2>&1; then
    echo "Building fxi index..."
    fxi index .
fi

# Start daemon for warm searches
echo -e "${GREEN}Starting fxi daemon...${NC}"
fxi daemon stop >/dev/null 2>&1 || true
fxi daemon start >/dev/null 2>&1
sleep 2

# Warm up the index with all test patterns
echo -e "${GREEN}Warming up fxi index...${NC}"
fxi "test" -l --color=never >/dev/null 2>&1 || true
fxi "NULL" -l --color=never >/dev/null 2>&1 || true
fxi "struct" -l --color=never >/dev/null 2>&1 || true
fxi "return" -l --color=never >/dev/null 2>&1 || true
fxi '"static void"' -l --color=never >/dev/null 2>&1 || true
fxi '"unsigned long"' -l --color=never >/dev/null 2>&1 || true
fxi '"struct device"' -l --color=never >/dev/null 2>&1 || true
fxi -i "error" -l --color=never >/dev/null 2>&1 || true
fxi -i "warning" -l --color=never >/dev/null 2>&1 || true
echo "Warmup complete."

# Function to run benchmark and capture time
run_benchmark() {
    local tool="$1"
    local cmd="$2"
    local total_time=0
    local count=0

    for i in $(seq 1 $RUNS); do
        # Use /usr/bin/time for precise timing
        result=$( { /usr/bin/time -p sh -c "$cmd > /tmp/bench_out_$$ 2>/dev/null" ; } 2>&1 )
        time_val=$(echo "$result" | grep "^real" | awk '{print $2}')
        total_time=$(echo "$total_time + $time_val" | bc)
    done

    count=$(wc -l < /tmp/bench_out_$$ | tr -d ' ')
    avg_time=$(echo "scale=3; $total_time / $RUNS" | bc)
    rm -f /tmp/bench_out_$$

    echo "$avg_time $count"
}

# Define test patterns
declare -a PATTERNS=(
    "simple:NULL"
    "simple:struct"
    "simple:return"
    "phrase:static void"
    "phrase:unsigned long"
    "phrase:struct device"
    "case:-i error"
    "case:-i warning"
)

# Start markdown output
cat > "$OUTPUT" << 'HEADER'
# fxi Benchmark Results

Automated benchmark comparing fxi and ripgrep.

## Environment

HEADER

echo "- **Codebase**: $CODEBASE_NAME" >> "$OUTPUT"
echo "- **Files**: $FILE_COUNT" >> "$OUTPUT"
echo "- **Date**: $(date '+%Y-%m-%d %H:%M:%S')" >> "$OUTPUT"
echo "- **fxi version**: $(fxi --version 2>/dev/null || echo 'N/A')" >> "$OUTPUT"
echo "- **ripgrep version**: $(rg --version | head -1)" >> "$OUTPUT"
echo "" >> "$OUTPUT"

# Get index stats
echo "### Index Statistics" >> "$OUTPUT"
echo '```' >> "$OUTPUT"
fxi stats . 2>/dev/null | head -15 >> "$OUTPUT"
echo '```' >> "$OUTPUT"
echo "" >> "$OUTPUT"

echo "## Results" >> "$OUTPUT"
echo "" >> "$OUTPUT"
echo "Times are averages of $RUNS runs. File counts shown for validation." >> "$OUTPUT"
echo "" >> "$OUTPUT"

# Table header
echo "| Pattern | fxi (ms) | rg (ms) | fxi files | rg files | Speedup vs rg |" >> "$OUTPUT"
echo "|---------|----------|---------|-----------|----------|---------------|" >> "$OUTPUT"

echo -e "${GREEN}Running benchmarks...${NC}"
echo ""

for pattern_spec in "${PATTERNS[@]}"; do
    type="${pattern_spec%%:*}"
    pattern="${pattern_spec#*:}"

    echo -e "Testing: ${YELLOW}$pattern${NC}"

    # Build commands based on type
    case "$type" in
        simple)
            fxi_cmd="fxi '$pattern' -l --color=never"
            rg_cmd="rg -l '$pattern' --no-ignore --color=never ."
            display_pattern="\`$pattern\`"
            ;;
        phrase)
            fxi_cmd="fxi '\"$pattern\"' -l --color=never"
            rg_cmd="rg -l '$pattern' --no-ignore --color=never ."
            display_pattern="\`\"$pattern\"\`"
            ;;
        case)
            flag="${pattern%% *}"
            term="${pattern#* }"
            fxi_cmd="fxi $flag '$term' -l --color=never"
            rg_cmd="rg $flag -l '$term' --no-ignore --color=never ."
            display_pattern="\`$flag $term\`"
            ;;
    esac

    # Run fxi benchmark
    fxi_result=$(run_benchmark "fxi" "$fxi_cmd")
    fxi_time=$(echo "$fxi_result" | awk '{print $1}')
    fxi_count=$(echo "$fxi_result" | awk '{print $2}')
    fxi_ms=$(echo "scale=0; $fxi_time * 1000 / 1" | bc)

    # Run ripgrep benchmark
    rg_result=$(run_benchmark "rg" "$rg_cmd")
    rg_time=$(echo "$rg_result" | awk '{print $1}')
    rg_count=$(echo "$rg_result" | awk '{print $2}')
    rg_ms=$(echo "scale=0; $rg_time * 1000 / 1" | bc)

    # Calculate speedup
    if [ "$fxi_ms" -gt 0 ]; then
        speedup=$(echo "scale=1; $rg_ms / $fxi_ms" | bc)
    else
        speedup="∞"
    fi

    # Output to markdown
    echo "| $display_pattern | $fxi_ms | $rg_ms | $fxi_count | $rg_count | **${speedup}x** |" >> "$OUTPUT"

    # Terminal output
    echo -e "  fxi: ${GREEN}${fxi_ms}ms${NC} ($fxi_count files)"
    echo -e "  rg:  ${YELLOW}${rg_ms}ms${NC} ($rg_count files)"
    echo -e "  Speedup: ${GREEN}${speedup}x${NC} vs ripgrep"
    echo ""
done

# Add notes section
cat >> "$OUTPUT" << NOTES

## Notes

- **fxi (warm)**: Using daemon with pre-loaded index
- **ripgrep**: Using \`--no-ignore\` to search all files (matching fxi's indexed files)
- All results are unlimited (full search)
- Times include I/O for writing results
- File counts may differ slightly due to gitignore handling

## Methodology

Each benchmark was run 3 times and averaged.

Commands used:
- fxi: \`fxi [pattern] -l --color=never\`
- ripgrep: \`rg -l [pattern] --no-ignore --color=never .\`
NOTES

echo -e "${GREEN}Benchmark complete!${NC}"
echo -e "Results written to: ${YELLOW}$OUTPUT${NC}"

# Stop daemon
fxi daemon stop >/dev/null 2>&1 || true
