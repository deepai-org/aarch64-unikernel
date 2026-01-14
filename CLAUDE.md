# aarch64 Unikernel for macOS Hypervisor.framework

## Current Status

**Hypervisor.framework VMM: WORKING** (with virtio-GPU graphics!)
**Virtualization.framework VMM: WORKING** (with virtio-GPU graphics + serial console!)

Both VMM approaches now have fully functional graphics and serial output!

### Latest Fixes (2025-01-13)

#### Fix #4: Console Output Priority (Race Condition)
The unikernel has two console drivers that can both initialize:
- `virtio_console.rs` - VZ-specific virtio-console driver
- `virtio_pci.rs` - Generic virtio-pci console driver

When `virtio_console` partially initialized but didn't work, `puts()` would silently fail while `print_hex()` worked (different code paths). This caused ~70% of boots to show only hex values without string labels.

**Solution:** Prefer `virtio_pci` over `virtio_console` since it's proven more reliable:
```rust
fn puts(s: &str) {
    // Prefer virtio_pci if available - proven more reliable
    if USE_VIRTIO_PCI {
        if let Some(ref console) = VIRTIO_PCI_CONSOLE {
            console.puts(s);
            return;
        }
    }
    // Try virtio-console (VZ) as fallback
    if virtio_console::console_available() { ... }
}
```

#### Fix #5: FPU/SIMD Enable at Boot
Rust compiler uses SIMD registers (q0, q1) for memcpy/memset. If FPU is disabled (default at reset), these cause silent traps.

**Solution:** Enable FPU in entry.s before any Rust code:
```asm
_start:
    mrs x1, cpacr_el1
    orr x1, x1, #(3 << 20)  // FPEN bits
    msr cpacr_el1, x1
    isb
```

#### Fix #6: Patience Scanner for GPU Detection
VZ GPU is a "heavy device" - requires Metal context, IOSurfaces, etc. Our kernel boots in microseconds but GPU may take 100ms+ to appear on PCI bus.

**Solution:** Scan ALL 32 PCI slots repeatedly for ~5 seconds:
```rust
for attempt in 1..=50 {
    for dev in 0u64..32 {
        // Look for VirtIO GPU (0x1AF4:0x1050 or 0x1040)
        if vendor_id == 0x1AF4 && (device_id == 0x1050 || device_id == 0x1040) {
            found_at_slot = dev;
            break;
        }
    }
    // Wait ~100ms between attempts
    for _ in 0..10_000_000 { spin_loop(); }
}
```

### Earlier Fixes

**VZ GPU NOW FULLY WORKING!** Three critical issues were fixed:

#### Fix #1: VZ Doesn't Program BARs
VZ expects Linux's PCI subsystem to program BAR registers. For bare-metal unikernels, BARs are all zero.

**Solution:** Program BAR0 ourselves before reading capabilities:
```rust
let bar0_ptr = (config_base + 0x10) as *mut u32;
let bar0_val = read_volatile(bar0_ptr);
if bar0_val == 0 {
    // GPU BAR0 address from Linux dmesg: 50008000-5000bfff
    write_volatile(bar0_ptr, 0x50008000);
}
```

#### Fix #2: VIRTIO_F_VERSION_1 Required (Bit 32)
VirtIO 1.0 **requires** the driver to set `VIRTIO_F_VERSION_1` (bit 32).

#### Fix #3: DON'T Accept All Device Features! (ROOT CAUSE OF BLACK SCREEN)
**This was the critical bug causing black screen.** We were accepting ALL features the device advertised:
```rust
// WRONG - accepts features we don't implement!
write_volatile(GF, device_features0);  // Bank 0
write_volatile(GF, device_features1 | 1);  // Bank 1
```

If the device advertises `VIRTIO_F_RING_PACKED`, `VIRTIO_F_ACCESS_PLATFORM`, or other advanced features and we accept them, the device expects behavior our simple split-ring driver doesn't provide.

**Solution:** Only accept features we actually implement:
```rust
// Bank 0: Accept ZERO features - we don't implement any optional features
write_volatile(GFSELECT, 0);
write_volatile(GF, 0);  // No features!

// Bank 1: ONLY accept VERSION_1
write_volatile(GFSELECT, 1);
write_volatile(GF, 1);  // Only VERSION_1!
```

### Other Important Fixes

#### Use Fixed RAM Addresses for DMA Buffers
Queue structures and framebuffer must be at known physical addresses accessible via DMA:
```rust
const QUEUE_RAM_BASE: u64 = 0x8000_0000;  // Queue structures
const HARDCODED_FB_ADDR: u64 = 0x7200_0000;  // Framebuffer
```

#### Use Volatile Operations for All Device-Visible Memory
All reads/writes to virtqueue structures must use `read_volatile`/`write_volatile`:
```rust
// WRONG - compiler may optimize away
GPU_QUEUE.used.idx

// RIGHT - forces memory access
read_volatile(&GPU_QUEUE.used.idx)
```

## VZ Memory Map (from /proc/iomem)

```
40000000-4fffffff : pci-host-generic 40000000.pci  (ECAM - PCI config space)
50000000-6ffdffff : PCI Bus 0000:00               (PCI MMIO - BAR regions)
  50000000-50003fff : 0000:00:01.0                (Network)
  50004000-50007fff : 0000:00:02.0                (Block?)
  50008000-5000bfff : 0000:00:06.0                (GPU BAR!)
70000000-afffffff : System RAM                    (1GB RAM)
```

## Architecture

### Two VMM Approaches

| VMM | API Level | DTB | Serial | Graphics | Status |
|-----|-----------|-----|--------|----------|--------|
| `hvf_vmm` | Low-level (Hypervisor.framework) | We provide | PL011 MMIO trap | MMIO virtio-gpu | **WORKING** |
| `vz_gui` | High-level (Virtualization.framework) | Auto-generated | virtio-console | PCI virtio-gpu | **WORKING** |

### Key Differences
- **HVF**: We control everything. PL011 UART at 0x09000000, virtio-GPU MMIO at 0x0a000000
- **VZ**: Black box. Uses virtio-console (not PL011), virtio-gpu via PCI (not MMIO)

## What Works

1. Kernel loads and executes in VZ
2. PCI ECAM access (0x40000000)
3. GPU device found at device 6
4. PCI capabilities parsed correctly
5. BAR0 programmed (0x50008000)
6. VirtIO feature negotiation (VERSION_1 only!)
7. FEATURES_OK accepted
8. Queue setup and DRIVER_OK
9. Display commands (GET_DISPLAY_INFO, RESOURCE_CREATE_2D, ATTACH_BACKING, SET_SCANOUT)
10. Framebuffer fill and drawing
11. TRANSFER_TO_HOST_2D and RESOURCE_FLUSH
12. **Graphics displayed on screen!**

## Debugging Strategy (No Console Output)

Since VZ uses virtio-console and we write to PL011, use crash-based debugging:

```rust
core::arch::asm!("brk #0xNN");  // Crash with specific number
```

- **VM crashes with "stopped unexpectedly"**: That breakpoint was hit
- **VM runs normally**: Code path didn't hit that breakpoint

### Response Code Checking
After GPU commands, check the response type to detect errors:
```rust
// 0x1100 = OK_NODATA, 0x1101 = OK_DISPLAY_INFO, 0x12xx = errors
let resp_type = read_volatile(&(*resp_hdr).cmd_type);
if resp_type >= 0x1200 {
    core::arch::asm!("brk #0xE0");  // Error!
}
```

## Files Structure

```
/Users/kevin/Desktop/uni/
├── CLAUDE.md
├── make_image.py         # Creates ARM64 Image header for VZ
├── my_unikernel/
│   ├── Cargo.toml
│   ├── linker.ld         # Links at 0x70000000
│   ├── src/
│   │   ├── main.rs       # Kernel entry, GPU init, patience scanner
│   │   ├── virtio.rs     # Virtio MMIO console driver
│   │   ├── virtio_pci.rs # Virtio PCI console driver (preferred)
│   │   ├── virtio_console.rs # Virtio-console driver for VZ
│   │   ├── virtio_gpu.rs # Virtio PCI GPU driver (for VZ)
│   │   ├── virtio_gpu_mmio.rs # Virtio MMIO GPU driver (for HVF)
│   │   ├── pci.rs        # PCI ECAM scanner, DTB parser
│   │   └── asm/entry.s   # Entry point, FPU enable, saves DTB
│   └── target/.../release/
│       ├── kernel        # ELF
│       ├── kernel.bin    # Raw binary (for HVF)
│       └── Image         # ARM64 Image (for VZ)
└── vmm/
    ├── hvf_vmm.swift     # Hypervisor.framework VMM (WORKING)
    ├── vz_gui.swift      # Virtualization.framework GUI VMM (WORKING)
    ├── hvf_entitlements.plist
    ├── entitlements.plist
    ├── linux-debian      # Debian ARM64 kernel
    └── initrd-debian     # Debian initramfs
```

## Build Commands

```bash
# Build kernel
cd /Users/kevin/Desktop/uni/my_unikernel
cargo build --release

# Create binaries
LLVM_BIN=~/.rustup/toolchains/nightly-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin
$LLVM_BIN/llvm-objcopy -O binary target/aarch64-unknown-none/release/kernel \
    target/aarch64-unknown-none/release/kernel.bin
python3 ../make_image.py target/aarch64-unknown-none/release/kernel.bin \
    target/aarch64-unknown-none/release/Image

# Run with HVF (working, with graphics!)
cd ../vmm
./hvf_vmm

# Run with VZ GUI (working, with graphics!)
./vz_gui
```

## Key Addresses

### HVF (we control)
- RAM_BASE: 0x70000000
- PL011_UART: 0x09000000
- Virtio-GPU MMIO: 0x0a000000

### VZ (from /proc/iomem)
- ECAM: 0x40000000 - 0x4FFFFFFF (PCI config space)
- PCI MMIO: 0x50000000 - 0x6FFDFFFF (BAR regions)
- GPU Config: 0x40030000 (ECAM + device 6 << 15)
- GPU BAR0: 0x50008000 (we program this ourselves!)
- RAM: 0x70000000 - 0xAFFFFFFF (1GB)
- Queue RAM: 0x80000000 (fixed address for DMA)
- Framebuffer: 0x72000000 (fixed address for DMA)

## VirtIO 1.0 Init Sequence (Must Follow Exactly)

1. Reset: Write 0 to Status
2. Ack: Write ACKNOWLEDGE (1)
3. Driver: Write DRIVER (2)
4. **Features Bank 0**: Write 0 (no optional features)
5. **Features Bank 1**: Write 1 (only VERSION_1)
6. Features OK: Write FEATURES_OK (8)
7. **CRITICAL**: Re-read Status, verify FEATURES_OK still set
8. Setup Queues (descriptors, avail, used rings at fixed RAM addresses)
9. Driver OK: Write DRIVER_OK (4)

## GPU Command Sequence

1. `GET_DISPLAY_INFO` - Get display dimensions (1280x720)
2. `RESOURCE_CREATE_2D` - Create resource with format B8G8R8X8_UNORM
3. `RESOURCE_ATTACH_BACKING` - Link resource to framebuffer RAM
4. `SET_SCANOUT` - Associate resource with display 0
5. Fill framebuffer with pixels
6. `TRANSFER_TO_HOST_2D` - Copy guest RAM to GPU
7. `RESOURCE_FLUSH` - Display the pixels

## Technical Notes

### ARM64 Linux Boot Protocol
- x0 = DTB pointer
- x1, x2, x3 = 0
- MMU off, caches off

### PCI ECAM Address Calculation
```
config_base = ecam_base + (bus << 20) + (device << 15) + (function << 12)
```
For GPU at 0000:00:06.0: `0x40000000 + 0 + 0x30000 + 0 = 0x40030000`

### VirtIO PCI Capability Structure
```
Offset  Size  Description
0x00    u8    cap_vndr (0x09 for VirtIO)
0x01    u8    cap_next
0x02    u8    cap_len
0x03    u8    cfg_type (1=Common, 2=Notify, 3=ISR, 4=Device)
0x04    u8    bar      <-- Which BAR holds this config
0x08    u32   offset   <-- Offset within that BAR
0x0C    u32   length
```

### Why BARs Need Manual Programming
VZ relies on the guest OS's PCI subsystem to:
1. Enumerate devices
2. Allocate BAR address space
3. Program BAR registers

Our unikernel skips all this, so we must program BARs ourselves using addresses from Linux dmesg.

### VZ Console Output
VZ uses virtio-console, NOT PL011 UART. We have two working drivers:
- `virtio_pci.rs` - Generic PCI console driver (preferred, more reliable)
- `virtio_console.rs` - VZ-specific virtio-console driver (fallback)

The VMM (`vz_gui.swift`) receives serial output and displays it with `[SERIAL]` prefix.

## VirtIO Driver Development (2025-01-14)

### BREAKTHROUGH: Linux-Like DTB-Based BAR Allocation

**All VirtIO devices now have valid BAR addresses!** The key was behaving exactly like Linux:

1. **Parse DTB `ranges` property** to find VZ's declared MMIO window
2. **Reserve addresses** VZ already programmed (Console at 0x5000C000)
3. **Allocate new addresses** within the valid window for unmapped devices

#### Implementation: Linux-Like Boot Sequence

```rust
// Phase 1: Parse DTB for valid MMIO window
let (mmio_base, mmio_size) = dtb::find_pci_mmio_window(dtb_ptr);
pci::init_allocator(mmio_base, mmio_size);

// Phase 2: Scan bus, reserve VZ's pre-programmed addresses
for slot in 0..32 {
    if let Some(dev) = PciDevice::new(ecam, 0, slot, 0) {
        dev.read_bars();
        for bar in dev.bars {
            if bar >= mmio_base && bar < mmio_limit {
                pci::reserve_range(bar, size);
            }
        }
    }
}

// Phase 3: Allocate BARs for unmapped devices
for slot in 0..32 {
    if dev.bars[i] == 0 && size > 0 {
        let addr = pci::allocate(size);
        dev.program_bar(i, addr);  // [OK] if VZ accepts!
    }
}

// Phase 4: Initialize drivers with allocated addresses
if let Some(modern) = VirtioModern::probe(&dev) {
    driver::from_modern(&modern);
}
```

#### DTB Parsing Results
```
MMIO Window: 0x50000000 - 0x6FFE0000 (511MB)
```

#### All BAR Allocations Accepted
| Device | ID | Slot | BAR0 | BAR2 | Status |
|--------|-----|------|------|------|--------|
| Network | 0x1041 | 1 | 0x50100000 | 0x50200000 | **[OK]** |
| Console | 0x1043 | 5 | 0x5000C000 (VZ) | 0x50300000 | **[OK]** |
| Block | 0x1042 | 6 | 0x50400000 | 0x50500000 | **[OK]** |
| GPU | 0x1050 | 7 | 0x50600000 | 0x50700000 | **[OK]** |
| Entropy | 0x1044 | 8 | 0x50800000 | 0x50900000 | **[OK]** |
| Balloon | 0x1045 | 9 | 0x50A00000 | 0x50B00000 | **[OK]** |

#### The Key Insight
VZ only accepts BAR writes for addresses **within the MMIO range declared in the DTB**. Writing to addresses outside this window (like 0x8000_0000 which is in RAM) is rejected.

### Why This Works (When Manual Addresses Failed)

1. **DTB declares the valid MMIO window** - VZ generates this from its internal configuration
2. **Reserve first, allocate second** - Avoid conflicts with VZ's pre-programmed devices
3. **Disable Memory Decode before programming** - Standard PCI BAR programming protocol
4. **Use 1MB alignment** - Conservative alignment ensures compatibility

### Files Structure

```
src/
├── dtb.rs         # DTB parser - finds MMIO window from `ranges` property
├── pci.rs         # Smart allocator + VirtioModern transport
├── main.rs        # Linux-like boot sequence orchestrator
├── virtio_*.rs    # Drivers with from_modern() for clean init
```

### Driver API: from_modern()

Drivers now accept a pre-configured `VirtioModern` transport:

```rust
impl VirtioEntropy {
    pub unsafe fn from_modern(modern: &VirtioModern, _ecam: u64) -> Option<Self> {
        // Device already has valid BAR addresses
        // Just configure queues and start
    }
}
```

## Lessons Learned

1. **Never accept all VirtIO features** - only accept what you implement
2. **Use fixed RAM addresses** for DMA buffers that both CPU and device access
3. **Use volatile operations** for all device-shared memory
4. **Check response codes** after every GPU command
5. **Enable FPU early** - Rust uses SIMD even for simple memcpy
6. **Heavy devices need patience** - GPU takes 100ms+ to appear, kernel boots in microseconds
7. **Consistent output paths** - all output functions should use the same driver priority
8. **Prefer proven drivers** - when multiple drivers can work, prioritize the reliable one
9. **Ghost Map addresses are configuration-dependent** - adding devices changes VZ's slot/BAR assignments
10. **Status = 0x00 after write = wrong address** - a quick diagnostic for address issues
