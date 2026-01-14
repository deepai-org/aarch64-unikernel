# aarch64 Unikernel for macOS

A minimal bare-metal unikernel for ARM64 that runs on macOS using Apple's virtualization frameworks. Features working virtio-GPU graphics and serial console output.

## Features

- Runs on both **Hypervisor.framework** (low-level) and **Virtualization.framework** (high-level)
- VirtIO GPU driver with 1280x720 display
- VirtIO serial console output
- No OS, no libc - pure bare-metal Rust
- ~14KB kernel image

## Screenshots

The unikernel displays colorful graphics demonstrating the working GPU driver.

## Requirements

- macOS (Apple Silicon)
- Rust nightly toolchain with `aarch64-unknown-none` target
- Xcode Command Line Tools

## Quick Start

```bash
# Install Rust target
rustup target add aarch64-unknown-none

# Build the kernel
cd my_unikernel
cargo build --release

# Create bootable image
LLVM_BIN=~/.rustup/toolchains/nightly-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin
$LLVM_BIN/llvm-objcopy -O binary target/aarch64-unknown-none/release/kernel \
    target/aarch64-unknown-none/release/kernel.bin
python3 ../make_image.py target/aarch64-unknown-none/release/kernel.bin \
    target/aarch64-unknown-none/release/Image

# Run with Virtualization.framework (GUI with graphics)
cd ../vmm
./vz_gui

# Or run with Hypervisor.framework (terminal output)
./hvf_vmm
```

## Project Structure

```
.
├── my_unikernel/           # The kernel
│   ├── src/
│   │   ├── main.rs         # Entry point, initialization
│   │   ├── virtio_gpu.rs   # VirtIO PCI GPU driver
│   │   ├── virtio_pci.rs   # VirtIO PCI console driver
│   │   ├── virtio_console.rs # VirtIO console driver
│   │   └── asm/entry.s     # Assembly entry point
│   ├── linker.ld           # Linker script
│   └── Cargo.toml
├── vmm/                    # Virtual Machine Monitors
│   ├── hvf_vmm.swift       # Hypervisor.framework VMM
│   ├── vz_gui.swift        # Virtualization.framework VMM
│   └── entitlements.plist
├── make_image.py           # Creates ARM64 boot image
├── CLAUDE.md               # Technical documentation
└── README.md
```

## How It Works

1. **Boot**: The VMM loads the kernel at 0x70000000 and jumps to `_start`
2. **Entry**: Assembly enables FPU, sets up stack, clears BSS, calls `kmain`
3. **PCI Scan**: Kernel scans PCI bus (ECAM at 0x40000000) for VirtIO devices
4. **Console Init**: Finds and initializes virtio-console for serial output
5. **GPU Init**: Finds virtio-GPU, negotiates features, sets up framebuffer
6. **Graphics**: Draws colorful rectangles to demonstrate working display
7. **Halt**: Enters WFI loop

## Technical Highlights

- **VirtIO 1.0**: Implements modern VirtIO with split virtqueues
- **PCI ECAM**: Direct PCI config space access without BIOS/UEFI
- **Manual BAR Programming**: VZ doesn't program BARs, so we do it ourselves
- **Fixed DMA Addresses**: Queue structures at known physical addresses
- **Patience Scanner**: GPU takes 100ms+ to appear, kernel polls repeatedly

## VMM Comparison

| Feature | hvf_vmm | vz_gui |
|---------|---------|--------|
| Framework | Hypervisor.framework | Virtualization.framework |
| Control Level | Low-level (traps) | High-level (config) |
| Serial | PL011 MMIO trap | virtio-console |
| GPU | virtio-gpu MMIO | virtio-gpu PCI |
| DTB | We provide | Auto-generated |

## Building the VMMs

```bash
cd vmm

# Build Hypervisor.framework VMM
swiftc -o hvf_vmm hvf_vmm.swift -framework Hypervisor
codesign -s - --entitlements hvf_entitlements.plist -f hvf_vmm

# Build Virtualization.framework VMM
swiftc -o vz_gui vz_gui.swift -framework Virtualization -framework AppKit
codesign -s - --entitlements entitlements.plist -f vz_gui
```

## License

MIT
