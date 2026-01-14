#![no_std]
#![no_main]

use core::panic::PanicInfo;
use core::ptr::{write_volatile, read_volatile};
use core::arch::global_asm;

mod virtio;
mod pci;
mod dtb;
mod virtio_pci;
mod virtio_gpu;
mod virtio_gpu_mmio;
mod virtio_console;
mod virtio_entropy;
mod virtio_block;
mod virtio_net;
mod virtio_balloon;

global_asm!(include_str!("asm/entry.s"));

// PL011 UART base (standard QEMU/HVF address)
pub static mut UART_BASE: usize = 0x0900_0000;

// Global state for console drivers
static mut USE_VIRTIO_PCI: bool = false;
static mut VIRTIO_PCI_CONSOLE: Option<virtio_pci::VirtioPciConsole> = None;

// PCI device storage for Linux-like boot sequence
static mut DEVICES: [Option<pci::PciDevice>; 32] = [None; 32];

/// Get BAR0 address for a VirtIO device (reads from PCI config space)
/// Used by legacy driver find() methods
pub fn get_virtio_bar0(config_base: u64, _device_id: u16) -> u64 {
    unsafe {
        let bar_val = read_volatile((config_base + 0x10) as *const u32);
        let is_64bit = (bar_val & 0x4) != 0;
        let mut addr = (bar_val & 0xFFFFFFF0) as u64;

        if is_64bit {
            let bar1_val = read_volatile((config_base + 0x14) as *const u32);
            addr |= (bar1_val as u64) << 32;
        }

        addr
    }
}

fn uart_putc(c: u8) {
    unsafe {
        write_volatile(UART_BASE as *mut u32, c as u32);
    }
}

fn putc(c: u8) {
    uart_putc(c);
    unsafe {
        if USE_VIRTIO_PCI {
            if let Some(ref console) = VIRTIO_PCI_CONSOLE {
                console.putc(c);
            }
        }
    }
    if virtio_console::console_available() {
        virtio_console::putc(c);
    }
}

fn puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            uart_putc(b'\r');
        }
        uart_putc(b);
    }
    unsafe {
        if USE_VIRTIO_PCI {
            if let Some(ref console) = VIRTIO_PCI_CONSOLE {
                console.puts(s);
            }
        }
    }
    if virtio_console::console_available() {
        virtio_console::puts(s);
    }
}

fn print_hex(n: u64) {
    let hex = b"0123456789ABCDEF";
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        buf[2 + i] = hex[((n >> ((15 - i) * 4)) & 0xF) as usize];
    }
    for &b in &buf {
        uart_putc(b);
    }
    unsafe {
        if USE_VIRTIO_PCI {
            if let Some(ref console) = VIRTIO_PCI_CONSOLE {
                console.write(&buf);
            }
        }
    }
    if virtio_console::console_available() {
        virtio_console::write_bytes(&buf);
    }
}

// Helper to draw demo graphics on MMIO GPU (for HVF)
fn draw_demo(gpu: &mut virtio_gpu_mmio::VirtioGpuMmio) {
    gpu.fill(0x001a1a2e);
    let colors = [0xFF5733, 0xFFC300, 0x28B463, 0x3498DB, 0x9B59B6];
    gpu.draw_rect(0, 0, gpu.width(), 40, 0x2c3e50);
    for (i, &color) in colors.iter().enumerate() {
        let x = 50 + (i as u32) * 140;
        gpu.draw_rect(x, 100, 120, 120, color);
    }
    for i in 0..5 {
        let x = 50 + (i as u32) * 140;
        gpu.draw_rect(x - 3, 97, 126, 3, 0xFFFFFF);
        gpu.draw_rect(x - 3, 220, 126, 3, 0xFFFFFF);
        gpu.draw_rect(x - 3, 97, 3, 126, 0xFFFFFF);
        gpu.draw_rect(x + 120, 97, 3, 126, 0xFFFFFF);
    }
}

#[no_mangle]
pub extern "C" fn kmain(dtb_ptr: u64) -> ! {
    let ecam: u64 = 0x40000000;

    // =========================================================================
    // PHASE 0: Bring up console first (patience scanner)
    // =========================================================================
    for _attempt in 1u32..=50 {
        let console_exists = unsafe {
            let mut found = false;
            for dev in 0u64..32 {
                let addr = ecam + (dev << 15);
                let header = read_volatile(addr as *const u32);
                let vendor_id = (header & 0xFFFF) as u16;
                let device_id = ((header >> 16) & 0xFFFF) as u16;
                if vendor_id == 0x1AF4 && (device_id == 0x1043 || device_id == 0x1003) {
                    found = true;
                    break;
                }
            }
            found
        };

        if console_exists && unsafe { !USE_VIRTIO_PCI } {
            if let Some(console) = virtio_pci::find_virtio_pci_console(ecam) {
                unsafe {
                    VIRTIO_PCI_CONSOLE = Some(console);
                    USE_VIRTIO_PCI = true;
                }
                break;
            }
        }

        if console_exists && unsafe { !USE_VIRTIO_PCI } && !virtio_console::console_available() {
            virtio_console::console_init();
            if virtio_console::console_available() {
                break;
            }
        }

        for _ in 0..1_000_000u64 {
            core::hint::spin_loop();
        }
    }

    // =========================================================================
    // Console is ready - begin output
    // =========================================================================
    puts("\n=== AArch64 VirtIO Unikernel ===\n");

    puts("DTB: ");
    print_hex(dtb_ptr);
    puts("\n");

    // =========================================================================
    // PHASE 1: Parse DTB for valid MMIO window
    // =========================================================================
    puts("\n--- Phase 1: Parse DTB ---\n");
    let (mmio_base, mmio_size) = unsafe {
        if let Some(window) = dtb::find_pci_mmio_window(dtb_ptr) {
            puts("MMIO Window: ");
            print_hex(window.0);
            puts(" - ");
            print_hex(window.0 + window.1);
            puts("\n");
            window
        } else {
            puts("DTB parse failed, using fallback\n");
            (0x5000_0000, 0x2000_0000)
        }
    };

    unsafe { pci::init_allocator(mmio_base, mmio_size); }

    // =========================================================================
    // PHASE 2: Scan bus and reserve VZ's pre-programmed addresses
    // =========================================================================
    puts("\n--- Phase 2: Scan & Reserve ---\n");
    unsafe {
        for slot in 0u8..32 {
            if let Some(mut dev) = pci::PciDevice::new(ecam, 0, slot, 0) {
                if dev.vendor_id == pci::VIRTIO_VENDOR_ID {
                    // Read existing BAR values
                    dev.read_bars();

                    // Reserve any valid addresses
                    for i in 0..6 {
                        let addr = dev.bars[i];
                        let (size, _is_64) = dev.get_bar_size(i);

                        if addr >= mmio_base && addr < (mmio_base + mmio_size) && size > 0 {
                            puts("Slot ");
                            print_hex(slot as u64);
                            puts(" Reserved BAR");
                            print_hex(i as u64);
                            puts(": ");
                            print_hex(addr);
                            puts("\n");
                            pci::reserve_range(addr, size);
                        }
                    }

                    DEVICES[slot as usize] = Some(dev);
                }
            }
        }
    }

    // =========================================================================
    // PHASE 3: Allocate missing BARs
    // =========================================================================
    puts("\n--- Phase 3: Allocate Missing ---\n");
    unsafe {
        for slot in 0u8..32 {
            if let Some(ref mut dev) = DEVICES[slot as usize] {
                // Skip GPU (0x1050, 0x1040) - let GPU driver handle its own BAR programming
                // The GPU driver knows the specific address VZ accepts (0x50008000)
                if dev.device_id == 0x1050 || dev.device_id == 0x1040 {
                    puts("Slot ");
                    print_hex(slot as u64);
                    puts(" (GPU) - skipped, driver handles BAR\n");
                    continue;
                }

                let mut i = 0usize;
                while i < 6 {
                    let (size, is_64) = dev.get_bar_size(i);

                    // If BAR has size but no address, allocate
                    if size > 0 && dev.bars[i] == 0 {
                        if let Some(addr) = pci::allocate(size) {
                            puts("Slot ");
                            print_hex(slot as u64);
                            puts(" (");
                            print_hex(dev.device_id as u64);
                            puts(") Alloc BAR");
                            print_hex(i as u64);
                            puts(" -> ");
                            print_hex(addr);

                            if dev.program_bar(i, addr) {
                                puts(" [OK]\n");
                            } else {
                                puts(" [FAIL]\n");
                            }
                        } else {
                            puts("Alloc failed for slot ");
                            print_hex(slot as u64);
                            puts("\n");
                        }
                    }

                    i += if is_64 { 2 } else { 1 };
                }
            }
        }
    }

    // =========================================================================
    // PHASE 4: Show final state (simplified to avoid probe hangs)
    // =========================================================================
    puts("\n--- Phase 4: Final State ---\n");
    unsafe {
        let (base, head, limit) = pci::get_allocator_state();
        puts("Allocator: ");
        print_hex(base);
        puts(" -> ");
        print_hex(head);
        puts("\n");
    }

    // =========================================================================
    // PHASE 5: Initialize GPU (patience scanner)
    // =========================================================================
    puts("\n--- Phase 5: GPU Init ---\n");
    let mut gpu_initialized = false;

    for attempt in 1u32..=50 {
        let mut found_slot: Option<u8> = None;

        for dev in 0u64..32 {
            unsafe {
                let addr = ecam + (dev << 15);
                let header = read_volatile(addr as *const u32);
                let vendor_id = (header & 0xFFFF) as u16;
                let device_id = ((header >> 16) & 0xFFFF) as u16;
                if vendor_id == 0x1AF4 && (device_id == 0x1050 || device_id == 0x1040) {
                    found_slot = Some(dev as u8);
                    break;
                }
            }
        }

        if let Some(slot) = found_slot {
            if let Some(mut gpu) = virtio_gpu::VirtioGpu::try_new(ecam, 0, slot) {
                puts("GPU found at slot ");
                print_hex(slot as u64);
                puts(" (attempt ");
                print_hex(attempt as u64);
                puts(")\n");

                if gpu.init_display() {
                    puts("Display: ");
                    print_hex(gpu.width() as u64);
                    puts("x");
                    print_hex(gpu.height() as u64);
                    puts("\n");

                    // Draw colorful pattern
                    gpu.fill(0xFFFFFFFF);
                    let colors = [0xFFFF5733, 0xFFFFC300, 0xFF28B463, 0xFF3498DB, 0xFF9B59B6];
                    gpu.draw_rect(0, 0, gpu.width(), 40, 0xFF2c3e50);
                    for (i, &color) in colors.iter().enumerate() {
                        let x = 50 + (i as u32) * 230;
                        gpu.draw_rect(x, 100, 200, 200, color);
                    }
                    for i in 0..5 {
                        let x = 50 + (i as u32) * 230;
                        gpu.draw_rect(x - 5, 95, 210, 5, 0xFFFFFFFF);
                        gpu.draw_rect(x - 5, 300, 210, 5, 0xFFFFFFFF);
                        gpu.draw_rect(x - 5, 95, 5, 210, 0xFFFFFFFF);
                        gpu.draw_rect(x + 200, 95, 5, 210, 0xFFFFFFFF);
                    }
                    gpu.flush();
                    puts("Graphics rendered!\n");

                    // Output test data
                    let samples = gpu.sample_test_pixels();
                    puts("TEST:PIXELS=");
                    for (i, &p) in samples.iter().enumerate() {
                        if i > 0 { puts(","); }
                        print_hex(p as u64);
                    }
                    puts("\n");

                    let all_black = samples.iter().all(|&p| p == 0);
                    puts("TEST:GRAPHICS=");
                    puts(if all_black { "FAIL\n" } else { "PASS\n" });

                    gpu_initialized = true;
                }
                break;
            }
        }

        if gpu_initialized {
            break;
        }

        for _ in 0..1_000_000u64 {
            core::hint::spin_loop();
        }
    }

    // Try MMIO GPU for HVF as fallback
    if !gpu_initialized {
        if let Some(mut gpu) = virtio_gpu_mmio::VirtioGpuMmio::try_new() {
            puts("Found MMIO GPU\n");
            if gpu.init_display() {
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

    // =========================================================================
    // PHASE 6: Test VirtIO Drivers
    // =========================================================================
    puts("\n--- Phase 6: Driver Tests ---\n");

    // Test Entropy
    puts("Testing Entropy...\n");
    unsafe {
        for slot in 0u8..32 {
            if let Some(ref dev) = DEVICES[slot as usize] {
                if dev.device_id == 0x1044 && dev.bars[0] != 0 {
                    if let Some(modern) = pci::VirtioModern::probe(dev) {
                        if let Some(entropy) = virtio_entropy::VirtioEntropy::from_modern(&modern, dev.ecam_addr) {
                            let stats = entropy.test_entropy();
                            puts("TEST:ENTROPY=");
                            puts(if stats.looks_random { "PASS" } else { "FAIL" });
                            puts(" (bytes=");
                            print_hex(stats.bytes_read as u64);
                            puts(", unique=");
                            print_hex(stats.unique_bytes as u64);
                            puts(")\n");
                            break;
                        }
                    }
                }
            }
        }
    }

    // Test Block
    puts("Testing Block...\n");
    unsafe {
        for slot in 0u8..32 {
            if let Some(ref dev) = DEVICES[slot as usize] {
                if dev.device_id == 0x1042 && dev.bars[0] != 0 {
                    if let Some(modern) = pci::VirtioModern::probe(dev) {
                        if let Some(block) = virtio_block::VirtioBlock::from_modern(&modern, dev.ecam_addr) {
                            puts("Capacity: ");
                            print_hex(block.capacity());
                            puts(" sectors\n");
                            let result = block.test_read_write();
                            puts("TEST:BLOCK=");
                            puts(if result.test_passed { "PASS" } else { "FAIL" });
                            puts(" (w=");
                            puts(if result.write_ok { "ok" } else { "fail" });
                            puts(",r=");
                            puts(if result.read_ok { "ok" } else { "fail" });
                            puts(",match=");
                            print_hex(result.data_matches as u64);
                            puts(")\n");
                            break;
                        }
                    }
                }
            }
        }
    }

    // Test Network
    puts("Testing Network...\n");
    unsafe {
        for slot in 0u8..32 {
            if let Some(ref dev) = DEVICES[slot as usize] {
                if dev.device_id == 0x1041 && dev.bars[0] != 0 {
                    if let Some(modern) = pci::VirtioModern::probe(dev) {
                        if let Some(net) = virtio_net::VirtioNet::from_modern(&modern, dev.ecam_addr) {
                            let mac = net.mac();
                            puts("MAC: ");
                            for (i, &b) in mac.iter().enumerate() {
                                if i > 0 { puts(":"); }
                                print_hex(b as u64);
                            }
                            puts("\n");
                            let result = net.test_network();
                            puts("TEST:NET=");
                            puts(if result.init_ok && result.send_ok { "PASS" } else { "FAIL" });
                            puts(" (send=");
                            puts(if result.send_ok { "ok" } else { "fail" });
                            puts(")\n");
                            break;
                        }
                    }
                }
            }
        }
    }

    // Test Balloon
    puts("Testing Balloon...\n");
    unsafe {
        for slot in 0u8..32 {
            if let Some(ref dev) = DEVICES[slot as usize] {
                if dev.device_id == 0x1045 && dev.bars[0] != 0 {
                    if let Some(modern) = pci::VirtioModern::probe(dev) {
                        if let Some(mut balloon) = virtio_balloon::VirtioBalloon::from_modern(&modern, dev.ecam_addr) {
                            let result = balloon.test_balloon();
                            puts("TEST:BALLOON=");
                            puts(if result.init_ok { "PASS" } else { "FAIL" });
                            puts(" (inflate=");
                            puts(if result.inflate_ok { "ok" } else { "fail" });
                            puts(",deflate=");
                            puts(if result.deflate_ok { "ok" } else { "fail" });
                            puts(")\n");
                            break;
                        }
                    }
                }
            }
        }
    }

    puts("\n=== All Tests Complete ===\n");
    puts("Halting.\n");

    loop {
        unsafe { core::arch::asm!("wfi"); }
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    puts("PANIC!\n");
    loop { unsafe { core::arch::asm!("wfi"); } }
}
