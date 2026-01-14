#!/bin/bash
set -e

echo "=== Building aarch64 Unikernel ==="

cd "$(dirname "$0")/my_unikernel"

# Ensure nightly toolchain with aarch64 target
echo "[1/4] Setting up Rust toolchain..."
rustup override set nightly 2>/dev/null || true
rustup target add aarch64-unknown-none 2>/dev/null || true
rustup component add rust-src llvm-tools-preview 2>/dev/null || true

# Build the kernel
echo "[2/4] Building kernel..."
cargo build --release -Z build-std=core,compiler_builtins -Z build-std-features=compiler-builtins-mem

# Check if build succeeded
if [ ! -f "target/aarch64-unknown-none/release/my_unikernel" ]; then
    echo "ERROR: Build failed - kernel binary not found"
    exit 1
fi

echo "[3/4] Kernel built successfully!"
ls -la target/aarch64-unknown-none/release/my_unikernel

# Show kernel info
echo ""
echo "[4/4] Kernel information:"
file target/aarch64-unknown-none/release/my_unikernel
size target/aarch64-unknown-none/release/my_unikernel 2>/dev/null || true

echo ""
echo "=== Build Complete ==="
echo "Kernel: $(pwd)/target/aarch64-unknown-none/release/my_unikernel"
