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

## VirtIO Driver Roadmap

### Implemented

| Driver | Device ID | Status | Notes |
|--------|-----------|--------|-------|
| virtio-gpu | 16 | Working | 1280x720 display, PCI and MMIO variants |
| virtio-console | 3 | Working | Serial I/O via virtio-pci |

### Planned (VZ-supported)

These devices are supported by macOS Virtualization.framework:

| Driver | Device ID | Priority | Complexity | VZ Class |
|--------|-----------|----------|------------|----------|
| **virtio-rng** | 4 | High | Very Low | `VZVirtioEntropyDeviceConfiguration` |
| **virtio-input** | 18 | High | Low | `VZVirtioKeyboardConfiguration`, `VZVirtioPointingDeviceConfiguration` |
| **virtio-blk** | 2 | High | Low | `VZVirtioBlockDeviceConfiguration` |
| **virtio-net** | 1 | High | Medium | `VZVirtioNetworkDeviceConfiguration` |
| **virtio-fs** | 26 | Medium | High | `VZVirtioFileSystemDeviceConfiguration` |
| **virtio-vsock** | 19 | Medium | Medium | `VZVirtioSocketDeviceConfiguration` |
| **virtio-sound** | 25 | Low | Medium | `VZVirtioSoundDeviceConfiguration` |
| **virtio-balloon** | 5 | Low | Low | `VZVirtioTraditionalMemoryBalloonDeviceConfiguration` |

### All VirtIO Device Types (Reference)

Complete list from the VirtIO specification:

| ID | Device | Description | VZ Support |
|----|--------|-------------|------------|
| 1 | net | Network card | Yes |
| 2 | blk | Block device (disk) | Yes |
| 3 | console | Serial console | Yes |
| 4 | rng | Entropy/random | Yes |
| 5 | balloon | Memory ballooning | Yes |
| 8 | scsi | SCSI host adapter | No |
| 9 | 9p | 9P filesystem | No |
| 16 | gpu | Graphics | Yes |
| 18 | input | Keyboard/mouse/tablet | Yes |
| 19 | vsock | Host-guest sockets | Yes |
| 20 | crypto | Cryptographic ops | No |
| 23 | iommu | IOMMU | No |
| 24 | mem | Memory hotplug | No |
| 25 | sound | Audio | Yes |
| 26 | fs | Filesystem sharing | Yes |
| 27 | pmem | Persistent memory | No |
| 29 | mac80211-hwsim | WiFi simulation | No |
| 30 | video | Video encode/decode | No |
| 40 | bt | Bluetooth | No |
| 41 | gpio | GPIO pins | No |
| 42 | i2c | I2C adapter | No |

### Implementation Notes

**virtio-rng** (Easiest)
- Single virtqueue, read-only
- Just submit buffer, device fills with random bytes
- No complex state machine

**virtio-input** (Keyboard/Mouse)
- Event-based input (EV_KEY, EV_REL, EV_ABS)
- Makes GPU interactive
- Need event queue processing

**virtio-blk** (Block Device)
- Request/response for read/write/flush
- Sector-based I/O (512 bytes)
- Foundation for filesystem

**virtio-net** (Networking)
- TX and RX virtqueues
- Ethernet frames
- Needs TCP/IP stack (e.g., smoltcp) for useful networking

**virtio-fs** (Filesystem)
- FUSE-based protocol
- Most complex - full filesystem semantics
- Share host directories with guest

### Dependencies

```
virtio-rng ─────────────────────────────> (standalone)

virtio-input ──────────────────────────-> virtio-gpu (interactive graphics)

virtio-blk ────> filesystem layer ─────-> (persistent storage)

virtio-net ────> TCP/IP stack (smoltcp) -> (networking)

virtio-fs ─────> FUSE protocol layer ───-> (host file access)
```

## Contributing

PRs welcome! See the driver roadmap above for what's needed. Each driver should:

1. Live in `my_unikernel/src/virtio_<device>.rs`
2. Follow VirtIO 1.0 spec (split virtqueues, feature negotiation)
3. Handle both PCI (for VZ) and optionally MMIO (for HVF)
4. Use fixed DMA addresses for queue structures

## License

MIT
