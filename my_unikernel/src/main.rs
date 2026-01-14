#![no_std]
#![no_main]

use core::panic::PanicInfo;
use core::ptr::{write_volatile, read_volatile};
use core::arch::global_asm;

mod virtio;
mod pci;
mod virtio_pci;
mod virtio_gpu;
mod virtio_gpu_mmio;
mod virtio_console;

global_asm!(include_str!("asm/entry.s"));

// PL011 UART - try multiple addresses
const UART_ADDRESSES: &[usize] = &[
    0x0900_0000,  // Standard QEMU/HVF
    0x0100_0000,  // Alternative
    0x0800_0000,  // Alternative
    0x1000_0000,  // High
    0x3F20_1000,  // Raspberry Pi
];

static mut UART_BASE: usize = 0x0900_0000;

// Debug output area - use a static buffer instead of absolute address
// (0x40000000 region is PCI ECAM in VZ, not RAM!)
static mut DEBUG_AREA: [u8; 256] = [0; 256];

fn debug_base() -> usize {
    unsafe { DEBUG_AREA.as_ptr() as usize }
}

const DEBUG_MAGIC: u32 = 0xDEAD_BEEF;

// Removed DIAG_BASE - was causing issues with absolute addresses

// Global state
static mut USE_VIRTIO: bool = false;
static mut USE_VIRTIO_PCI: bool = false;
static mut VIRTIO_CONSOLE: Option<virtio::VirtioConsole> = None;
static mut VIRTIO_PCI_CONSOLE: Option<virtio_pci::VirtioPciConsole> = None;
static mut FOUND_VIRTIO_ADDR: u64 = 0;

// PCI scan results for debug display
static mut PCI_DEVICES: [(u16, u16); 32] = [(0, 0); 32];
static mut PCI_COUNT: usize = 0;

fn uart_putc(c: u8) {
    unsafe {
        write_volatile(UART_BASE as *mut u32, c as u32);
    }
}

fn putc(c: u8) {
    // 1. ALWAYS write to UART first (stateless, cannot hang)
    uart_putc(c);

    // 2. ALSO write to VirtIO PCI if available
    unsafe {
        if USE_VIRTIO_PCI {
            if let Some(ref console) = VIRTIO_PCI_CONSOLE {
                console.putc(c);
            }
        }
    }

    // 3. ALSO write to VirtIO Console if available
    if virtio_console::console_available() {
        virtio_console::putc(c);
    }
}

fn puts(s: &str) {
    // 1. Send to UART char-by-char (stateless, reliable)
    for b in s.bytes() {
        if b == b'\n' {
            uart_putc(b'\r');
        }
        uart_putc(b);
    }

    // 2. Send to VirtIO PCI (batch)
    unsafe {
        if USE_VIRTIO_PCI {
            if let Some(ref console) = VIRTIO_PCI_CONSOLE {
                console.puts(s);
            }
        }
    }

    // 3. Send to VirtIO Console (batch)
    if virtio_console::console_available() {
        virtio_console::puts(s);
    }
}

fn print_hex(n: u64) {
    let hex = b"0123456789ABCDEF";
    let mut buf = [0u8; 18]; // "0x" + 16 hex digits
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        buf[2 + i] = hex[((n >> ((15 - i) * 4)) & 0xF) as usize];
    }

    // 1. Send to UART (stateless)
    for &b in &buf {
        uart_putc(b);
    }

    // 2. Send to VirtIO PCI (batch)
    unsafe {
        if USE_VIRTIO_PCI {
            if let Some(ref console) = VIRTIO_PCI_CONSOLE {
                console.write(&buf);
            }
        }
    }

    // 3. Send to VirtIO Console (batch)
    if virtio_console::console_available() {
        virtio_console::write_bytes(&buf);
    }
}

unsafe fn debug_write_u32(offset: usize, val: u32) {
    write_volatile((debug_base() + offset) as *mut u32, val);
}

unsafe fn debug_write_u64(offset: usize, val: u64) {
    write_volatile((debug_base() + offset) as *mut u64, val);
}

unsafe fn debug_write_str(offset: usize, s: &str) {
    let base = (debug_base() + offset) as *mut u8;
    for (i, b) in s.bytes().enumerate() {
        write_volatile(base.add(i), b);
    }
    write_volatile(base.add(s.len()), 0);
}

// Simple DTB parser - look for virtio-mmio or virtio device addresses
unsafe fn find_virtio_from_dtb(dtb_ptr: u64) -> Option<u64> {
    if dtb_ptr == 0 {
        return None;
    }

    // Check DTB magic
    let magic = read_volatile(dtb_ptr as *const u32).swap_bytes();
    if magic != 0xd00dfeed {
        return None;
    }

    let total_size = read_volatile((dtb_ptr + 4) as *const u32).swap_bytes() as usize;
    let off_dt_struct = read_volatile((dtb_ptr + 8) as *const u32).swap_bytes() as u64;
    let off_dt_strings = read_volatile((dtb_ptr + 12) as *const u32).swap_bytes() as u64;

    let struct_base = dtb_ptr + off_dt_struct;
    let strings_base = dtb_ptr + off_dt_strings;

    // Walk the structure block looking for virtio nodes
    let mut pos: u64 = 0;
    let max_pos = (total_size as u64).saturating_sub(off_dt_struct);

    while pos < max_pos {
        let token = read_volatile((struct_base + pos) as *const u32).swap_bytes();
        pos += 4;

        match token {
            0x1 => {
                // FDT_BEGIN_NODE - node name follows
                // Skip the name (null-terminated, padded to 4 bytes)
                let name_start = struct_base + pos;
                let mut name_len = 0u64;
                while name_len < 256 {
                    let c = read_volatile((name_start + name_len) as *const u8);
                    if c == 0 {
                        break;
                    }
                    name_len += 1;
                }
                // Pad to 4-byte boundary
                pos += (name_len + 4) & !3;
            }
            0x2 => {
                // FDT_END_NODE
            }
            0x3 => {
                // FDT_PROP - property
                let len = read_volatile((struct_base + pos) as *const u32).swap_bytes();
                let nameoff = read_volatile((struct_base + pos + 4) as *const u32).swap_bytes();
                pos += 8;

                // Get property name
                let name_ptr = strings_base + nameoff as u64;
                let prop_name = read_str(name_ptr);

                // Check if this is a "reg" property - might contain device address
                if prop_name == "reg" && len >= 8 {
                    let addr_hi = read_volatile((struct_base + pos) as *const u32).swap_bytes() as u64;
                    let addr_lo = read_volatile((struct_base + pos + 4) as *const u32).swap_bytes() as u64;
                    let addr = (addr_hi << 32) | addr_lo;

                    // Check if this looks like a virtio MMIO address
                    if addr != 0 && addr < 0x1_0000_0000 {
                        // Try to probe this as virtio
                        if let Some(_) = virtio::VirtioConsole::try_new(addr as usize) {
                            return Some(addr);
                        }
                    }
                }

                // Pad to 4-byte boundary
                pos += ((len as u64) + 3) & !3;
            }
            0x9 => {
                // FDT_END
                break;
            }
            _ => {
                // Unknown token, try to continue
            }
        }
    }

    None
}

unsafe fn read_str(ptr: u64) -> &'static str {
    let mut len = 0;
    while len < 64 {
        let c = read_volatile((ptr + len as u64) as *const u8);
        if c == 0 {
            break;
        }
        len += 1;
    }
    let slice = core::slice::from_raw_parts(ptr as *const u8, len);
    core::str::from_utf8_unchecked(slice)
}

// Helper to draw demo graphics on MMIO GPU
fn draw_demo(gpu: &mut virtio_gpu_mmio::VirtioGpuMmio) {
    gpu.fill(0x001a1a2e); // Dark blue background

    // Draw a colorful pattern
    let colors = [
        0xFF5733, // Red-orange
        0xFFC300, // Yellow
        0x28B463, // Green
        0x3498DB, // Blue
        0x9B59B6, // Purple
    ];

    // Title bar
    gpu.draw_rect(0, 0, gpu.width(), 40, 0x2c3e50);

    // Draw colorful boxes
    for (i, &color) in colors.iter().enumerate() {
        let x = 50 + (i as u32) * 140;
        let y = 100;
        gpu.draw_rect(x, y, 120, 120, color);
    }

    // Draw borders around boxes
    for i in 0..5 {
        let x = 50 + (i as u32) * 140;
        gpu.draw_rect(x - 3, 97, 126, 3, 0xFFFFFF);  // Top
        gpu.draw_rect(x - 3, 220, 126, 3, 0xFFFFFF); // Bottom
        gpu.draw_rect(x - 3, 97, 3, 126, 0xFFFFFF);  // Left
        gpu.draw_rect(x + 120, 97, 3, 126, 0xFFFFFF); // Right
    }

    // Draw a smiley face
    let cx = 400u32;
    let cy = 400u32;

    // Face circle (approximate with rects)
    for dy in 0..80i32 {
        for dx in 0..80i32 {
            let dist = ((dx - 40) * (dx - 40) + (dy - 40) * (dy - 40)) as u32;
            if dist < 1600 && dist > 1200 {
                gpu.draw_rect(cx - 40 + dx as u32, cy - 40 + dy as u32, 1, 1, 0xFFD700);
            }
        }
    }

    // Eyes
    gpu.draw_rect(cx - 15, cy - 15, 8, 8, 0xFFD700);
    gpu.draw_rect(cx + 8, cy - 15, 8, 8, 0xFFD700);

    // Smile
    for i in 0..24u32 {
        let x = cx - 12 + i;
        let y = cy + 8 + (if i < 12 { i / 3 } else { (24 - i) / 3 });
        gpu.draw_rect(x, y, 2, 2, 0xFFD700);
    }
}

/// Register Canary: Scan all PCI devices and crash with device info in registers
/// brk #1 = found virtio device (x0=dev, x1=device_id, x2=addr)
/// brk #2 = no virtio found (x0=0xFF)
#[inline(never)]
#[no_mangle]
pub unsafe fn debug_scan_pci(ecam_base: u64) -> ! {
    // Scan slots 0 to 31
    for dev in 0u64..32 {
        // Calculate config address for Bus 0, Device 'dev', Function 0
        let addr = ecam_base + (dev << 15);

        // Read 32 bits at offset 0 (Vendor ID + Device ID)
        let header = core::ptr::read_volatile(addr as *const u32);
        let vendor_id = (header & 0xFFFF) as u16;
        let device_id = (header >> 16) as u16;

        // Check for VirtIO Vendor ID (0x1AF4)
        if vendor_id == 0x1AF4 {
            // FOUND IT! Crash with device info in registers
            // x0 = Device Number
            // x1 = Device ID (e.g. 0x1050 for GPU)
            // x2 = Address found
            core::arch::asm!(
                "mov x0, {0}",
                "mov x1, {1}",
                "mov x2, {2}",
                "brk #1",
                in(reg) dev,
                in(reg) device_id as u64,
                in(reg) addr,
            );
            loop { core::arch::asm!("wfi"); }
        }
    }

    // If we get here, we found NOTHING
    core::arch::asm!("mov x0, #0xFF", "brk #2");
    loop { core::arch::asm!("wfi"); }
}

#[no_mangle]
pub extern "C" fn kmain(dtb_ptr: u64) -> ! {
    // No blind delays! They starve VZ device initialization threads.

    let ecam: u64 = 0x40000000;

    // -----------------------------------------------------------------------
    // PATIENCE SCANNER FIRST: Bring up console BEFORE any output!
    // VZ devices take time to appear - run this before trying to print anything.
    // -----------------------------------------------------------------------
    for attempt in 1u32..=50 {
        // Check if console device exists on bus
        let console_exists = unsafe {
            let mut found = false;
            for dev in 0u64..32 {
                let addr = ecam + (dev << 15);
                let header = core::ptr::read_volatile(addr as *const u32);
                let vendor_id = (header & 0xFFFF) as u16;
                let device_id = ((header >> 16) & 0xFFFF) as u16;
                // Console device IDs: 0x1043 (modern) or 0x1003 (legacy)
                if vendor_id == 0x1AF4 && (device_id == 0x1043 || device_id == 0x1003) {
                    found = true;
                    break;
                }
            }
            found
        };

        // Try virtio_pci console init
        if console_exists && unsafe { !USE_VIRTIO_PCI } {
            if let Some(console) = virtio_pci::find_virtio_pci_console(ecam) {
                unsafe {
                    VIRTIO_PCI_CONSOLE = Some(console);
                    USE_VIRTIO_PCI = true;
                }
                // Console is up! We can start outputting now.
                break;
            }
        }

        // Try virtio_console as fallback ONLY if virtio_pci failed
        // (both drivers target the same device - double init kills it!)
        if console_exists && unsafe { !USE_VIRTIO_PCI } && !virtio_console::console_available() {
            virtio_console::console_init();
            if virtio_console::console_available() {
                // Console is up!
                break;
            }
        }

        // Wait ~10ms before retry
        for _ in 0..1_000_000u64 {
            core::hint::spin_loop();
        }
    }

    // Now console should be up (if device exists). Safe to output.

    // Collect PCI device info for display on GPU later
    unsafe {
        core::arch::asm!("dsb sy");
        let ecam_base: u64 = 0x40000000;
        for dev in 0u64..32 {
            let addr = ecam_base + (dev << 15);
            let header = core::ptr::read_volatile(addr as *const u32);
            let vendor_id = (header & 0xFFFF) as u16;
            let device_id = ((header >> 16) & 0xFFFF) as u16;

            if vendor_id != 0 && vendor_id != 0xFFFF {
                PCI_DEVICES[PCI_COUNT] = (vendor_id, device_id);
                PCI_COUNT += 1;
            }
        }
    }

    unsafe {
        debug_write_u32(0, DEBUG_MAGIC);
        debug_write_u64(8, dtb_ptr);
    }

    puts("\n=== aarch64 Unikernel ===\n");

    // Report which console driver is active
    if virtio_console::console_available() {
        puts("Output: virtio-console (VZ)\n");
    } else {
        unsafe {
            if USE_VIRTIO_PCI {
                puts("Output: virtio-pci\n");
            } else {
                puts("Output: No VirtIO console\n");
            }
        }
    }

    puts("DTB: ");
    print_hex(dtb_ptr);
    puts("\n");

    // Check DTB magic to verify it's valid
    if dtb_ptr != 0 {
        let dtb_magic = unsafe { core::ptr::read_volatile(dtb_ptr as *const u32) };
        puts("DTB magic: ");
        print_hex(dtb_magic.swap_bytes() as u64);  // DTB is big-endian
        puts("\n");
    }

    // CPU info
    let midr: u64;
    let current_el: u64;
    unsafe {
        core::arch::asm!("mrs {}, MIDR_EL1", out(reg) midr);
        core::arch::asm!("mrs {}, CurrentEL", out(reg) current_el);
    }

    puts("MIDR: ");
    print_hex(midr);
    puts("\n");

    puts("EL: ");
    putc(b'0' + ((current_el >> 2) & 0x3) as u8);
    puts("\n");

    // Try to initialize GPU with PATIENCE SCANNER
    // GPU is "heavy" - VZ needs to spin up Metal context, IOSurfaces, etc.
    // This can take 100ms+ while our kernel boots in microseconds.
    puts("Looking for GPU...\n");

    let mut gpu_initialized = false;
    let mut gpu_result: Option<virtio_gpu::VirtioGpu> = None;

    // -----------------------------------------------------------------------
    // GPU PATIENCE SCANNER: Scan ALL slots, repeatedly
    // -----------------------------------------------------------------------
    puts("Scanning for GPU (Patience Mode)...\n");

    // Try for ~5 seconds
    for attempt in 1u32..=50 {
        let mut found_at_slot: u64 = 0xFF;

        // Scan ALL 32 slots every time!
        for dev in 0u64..32 {
            unsafe {
                let addr = ecam + (dev << 15);
                let header = core::ptr::read_volatile(addr as *const u32);
                let vendor_id = (header & 0xFFFF) as u16;
                let device_id = ((header >> 16) & 0xFFFF) as u16;

                // Check for VirtIO (0x1AF4) + GPU (0x1050 or 0x1040)
                if vendor_id == 0x1AF4 && (device_id == 0x1050 || device_id == 0x1040) {
                    found_at_slot = dev;
                    break;
                }
            }
        }

        if found_at_slot != 0xFF {
            puts("GPU at slot ");
            print_hex(found_at_slot);
            puts(" attempt ");
            print_hex(attempt as u64);
            puts("\n");

            // Initialize using the slot we JUST found
            if let Some(gpu) = virtio_gpu::VirtioGpu::try_new(ecam, 0, found_at_slot as u8) {
                gpu_result = Some(gpu);
                break;
            } else {
                puts("try_new failed - retrying...\n");
            }
        }

        if gpu_result.is_some() {
            break;
        }

        // Not found yet? Wait and retry.
        if attempt % 10 == 0 {
            puts("Attempt ");
            print_hex(attempt as u64);
            puts(" - scanning bus:\n");
            // Dump what IS on the bus to debug
            unsafe {
                for dev in 0u64..32 {
                    let addr = ecam + (dev << 15);
                    let header = core::ptr::read_volatile(addr as *const u32);
                    let vid = (header & 0xFFFF) as u16;
                    let did = ((header >> 16) & 0xFFFF) as u16;
                    if vid != 0xFFFF && vid != 0 {
                        puts("  Slot ");
                        print_hex(dev);
                        puts(": ");
                        print_hex(vid as u64);
                        puts(":");
                        print_hex(did as u64);
                        puts("\n");
                    }
                }
            }
        }

        // Wait ~10ms (deterministic delay)
        for _ in 0..1_000_000u64 {
            core::hint::spin_loop();
        }
    }

    if gpu_result.is_none() {
        puts("GPU timeout after 50 attempts\n");
    }

    if let Some(mut gpu) = gpu_result {
        puts("Found virtio-gpu!\n");
        if gpu.init_display() {
            puts("Display initialized: ");
            print_hex(gpu.width() as u64);
            puts("x");
            print_hex(gpu.height() as u64);
            puts("\n");

            // Fill with WHITE - guaranteed visible regardless of format
            gpu.fill(0xFFFFFFFF);
            gpu.flush();
            puts("Filled white!\n");

            // Draw a colorful pattern
            let colors = [
                0xFFFF5733, // Red-orange
                0xFFFFC300, // Yellow
                0xFF28B463, // Green
                0xFF3498DB, // Blue
                0xFF9B59B6, // Purple
            ];

            // Title bar
            gpu.draw_rect(0, 0, gpu.width(), 40, 0xFF2c3e50);

            // Draw colorful boxes
            for (i, &color) in colors.iter().enumerate() {
                let x = 50 + (i as u32) * 230;
                gpu.draw_rect(x, 100, 200, 200, color);
            }

            // Draw borders
            for i in 0..5 {
                let x = 50 + (i as u32) * 230;
                gpu.draw_rect(x - 5, 95, 210, 5, 0xFFFFFFFF);
                gpu.draw_rect(x - 5, 300, 210, 5, 0xFFFFFFFF);
                gpu.draw_rect(x - 5, 95, 5, 210, 0xFFFFFFFF);
                gpu.draw_rect(x + 200, 95, 5, 210, 0xFFFFFFFF);
            }

            gpu.flush();
            puts("Graphics rendered!\n");

            // Output test verification data
            let (checksum, non_zero) = gpu.framebuffer_checksum();
            puts("TEST:FB_CHECKSUM=");
            print_hex(checksum as u64);
            puts("\n");
            puts("TEST:FB_NONZERO=");
            print_hex(non_zero as u64);
            puts("\n");

            // Sample pixels should be non-zero (colored boxes drawn)
            let samples = gpu.sample_test_pixels();
            puts("TEST:PIXELS=");
            for (i, &p) in samples.iter().enumerate() {
                if i > 0 { puts(","); }
                print_hex(p as u64);
            }
            puts("\n");

            // Verify pixels are not all black
            let all_black = samples.iter().all(|&p| p == 0);
            if all_black {
                puts("TEST:GRAPHICS=FAIL (all black)\n");
            } else {
                puts("TEST:GRAPHICS=PASS\n");
            }

            gpu_initialized = true;
        }
    }

    // SECOND: Try MMIO-based virtio-GPU (for HVF VMM) as fallback
    if !gpu_initialized {
        if let Some(mut gpu) = virtio_gpu_mmio::VirtioGpuMmio::try_new() {
            puts("Found virtio-GPU MMIO!\n");
            if gpu.init_display() {
                puts("MMIO Display: ");
                print_hex(gpu.width() as u64);
                puts("x");
                print_hex(gpu.height() as u64);
                puts("\n");

                // Draw demo pattern
                draw_demo(&mut gpu);
                gpu.flush();
                puts("MMIO Graphics rendered!\n");
                gpu_initialized = true;
            }
        }
    }

    if !gpu_initialized {
        puts("No GPU found\n");
    }

    puts("Halting.\n");

    unsafe {
        debug_write_u32(252, 0xCAFEBABE);
    }

    loop {
        unsafe { core::arch::asm!("wfi"); }
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    puts("PANIC!\n");
    loop { unsafe { core::arch::asm!("wfi"); } }
}
