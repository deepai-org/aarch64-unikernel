#!/bin/bash
# Automated test runner for the aarch64 unikernel
# Usage: ./test.sh [--skip-build] [--verbose]

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KERNEL_DIR="$SCRIPT_DIR/my_unikernel"
VMM_DIR="$SCRIPT_DIR/vmm"
LLVM_BIN=~/.rustup/toolchains/nightly-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

SKIP_BUILD=false
VERBOSE=false

# Parse args
for arg in "$@"; do
    case $arg in
        --skip-build)
            SKIP_BUILD=true
            ;;
        --verbose)
            VERBOSE=true
            ;;
        *)
            echo "Unknown argument: $arg"
            echo "Usage: $0 [--skip-build] [--verbose]"
            exit 1
            ;;
    esac
done

echo "========================================"
echo "  aarch64 Unikernel Test Suite"
echo "========================================"
echo ""

# Step 1: Build kernel
if [ "$SKIP_BUILD" = false ]; then
    echo -e "${YELLOW}[1/4] Building kernel...${NC}"
    cd "$KERNEL_DIR"
    cargo build --release 2>&1 | grep -E "(Compiling|Finished|error)" || true

    if [ ${PIPESTATUS[0]} -ne 0 ]; then
        echo -e "${RED}Build failed!${NC}"
        exit 1
    fi
    echo -e "${GREEN}Build successful${NC}"
    echo ""
else
    echo -e "${YELLOW}[1/4] Skipping build (--skip-build)${NC}"
    echo ""
fi

# Step 2: Create Image
if [ "$SKIP_BUILD" = false ]; then
    echo -e "${YELLOW}[2/4] Creating boot image...${NC}"
    cd "$KERNEL_DIR"

    $LLVM_BIN/llvm-objcopy -O binary \
        target/aarch64-unknown-none/release/kernel \
        target/aarch64-unknown-none/release/kernel.bin

    python3 "$SCRIPT_DIR/make_image.py" \
        target/aarch64-unknown-none/release/kernel.bin \
        target/aarch64-unknown-none/release/Image

    echo -e "${GREEN}Image created${NC}"
    echo ""
else
    echo -e "${YELLOW}[2/4] Skipping image creation${NC}"
    echo ""
fi

# Step 3: Build test VMM if needed
echo -e "${YELLOW}[3/4] Building test VMM...${NC}"
cd "$VMM_DIR"

if [ ! -f vz_test ] || [ vz_test.swift -nt vz_test ]; then
    swiftc -O -o vz_test vz_test.swift \
        -framework Virtualization \
        -framework Foundation 2>&1

    codesign -s - --entitlements entitlements.plist -f vz_test
    echo -e "${GREEN}Test VMM built${NC}"
else
    echo "Test VMM up to date"
fi
echo ""

# Step 4: Run tests
echo -e "${YELLOW}[4/4] Running tests...${NC}"
echo ""

cd "$VMM_DIR"
export KERNEL_PATH="$KERNEL_DIR/target/aarch64-unknown-none/release/Image"

# Run the test
if [ "$VERBOSE" = true ]; then
    ./vz_test
    TEST_EXIT=$?
else
    # Capture output but show progress
    ./vz_test 2>&1 | tee /tmp/unikernel_test.log | grep -E "(===|✓|✗|SERIAL.*Unikernel|SERIAL.*GPU|SERIAL.*Display|SERIAL.*Graphics|SERIAL.*Halting|TEST)"
    TEST_EXIT=${PIPESTATUS[0]}
fi

echo ""
echo "========================================"
if [ $TEST_EXIT -eq 0 ]; then
    echo -e "${GREEN}  ALL TESTS PASSED ✓${NC}"
else
    echo -e "${RED}  TESTS FAILED ✗${NC}"
    if [ "$VERBOSE" = false ]; then
        echo ""
        echo "Run with --verbose for full output"
        echo "Log saved to /tmp/unikernel_test.log"
    fi
fi
echo "========================================"

exit $TEST_EXIT
