//! Linux-like PCI ECAM scanner with DTB-based BAR allocation
//!
//! This implements proper PCI resource allocation like Linux:
//! 1. Parse DTB to find the exact MMIO window VZ has authorized
//! 2. Scan bus to find VZ's pre-programmed addresses (Console/GPU)
//! 3. Reserve those addresses in our allocator
//! 4. Allocate BARs for unmapped devices (Network/Balloon) within the valid window

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// Virtio vendor ID
pub const VIRTIO_VENDOR_ID: u16 = 0x1af4;

// PCI config space offsets
const PCI_COMMAND: usize = 0x04;
const PCI_STATUS: usize = 0x06;
const PCI_BAR0: usize = 0x10;
const PCI_CAP_PTR: usize = 0x34;

// DTB-based MMIO allocator state
static mut MMIO_BASE: u64 = 0;
static mut MMIO_LIMIT: u64 = 0;
static mut MMIO_HEAD: u64 = 0;
static mut ALLOCATOR_INITIALIZED: bool = false;

/// Initialize the MMIO allocator with the window from DTB
pub unsafe fn init_allocator(base: u64, size: u64) {
    MMIO_BASE = base;
    MMIO_LIMIT = base + size;
    MMIO_HEAD = base;
    ALLOCATOR_INITIALIZED = true;
}

/// Reserve a range that's already in use (by VZ-mapped devices)
/// This bumps the allocator past any existing device
pub unsafe fn reserve_range(addr: u64, size: u64) {
    if !ALLOCATOR_INITIALIZED {
        return;
    }

    let end = addr + size;
    // If this range extends past current pointer, bump it
    if end > MMIO_HEAD && end <= MMIO_LIMIT {
        // Align to 1MB boundary for safety
        MMIO_HEAD = (end + 0xFFFFF) & !0xFFFFF;
    }
}

/// Allocate a new MMIO range for a BAR
pub unsafe fn allocate(size: u64) -> Option<u64> {
    if !ALLOCATOR_INITIALIZED || size == 0 {
        return None;
    }

    // Align to 1MB (PCI BARs must be naturally aligned, 1MB is safe minimum)
    let align = 0x100000u64;
    let start = (MMIO_HEAD + align - 1) & !(align - 1);
    let end = start + size;

    if end <= MMIO_LIMIT {
        MMIO_HEAD = end;
        Some(start)
    } else {
        None
    }
}

/// Get current allocator state for debugging
pub unsafe fn get_allocator_state() -> (u64, u64, u64) {
    (MMIO_BASE, MMIO_HEAD, MMIO_LIMIT)
}

#[derive(Clone, Copy)]
pub struct PciDevice {
    pub ecam_addr: u64,
    pub vendor_id: u16,
    pub device_id: u16,
    pub bars: [u64; 6],
}

impl PciDevice {
    /// Create a PCI device (does NOT read BARs - call read_bar separately)
    pub unsafe fn new(ecam_base: u64, bus: u8, slot: u8, func: u8) -> Option<Self> {
        let addr = ecam_base
            + ((bus as u64) << 20)
            + ((slot as u64) << 15)
            + ((func as u64) << 12);

        // Read vendor/device ID
        let header = read_volatile(addr as *const u32);
        let vendor = (header & 0xFFFF) as u16;
        if vendor == 0xFFFF || vendor == 0 {
            return None;
        }
        let device = (header >> 16) as u16;

        Some(PciDevice {
            ecam_addr: addr,
            vendor_id: vendor,
            device_id: device,
            bars: [0; 6],
        })
    }

    /// Read a single BAR's current address
    pub unsafe fn read_bar(&self, idx: usize) -> u64 {
        if idx >= 6 {
            return 0;
        }

        let offset = PCI_BAR0 + (idx * 4);
        let val = read_volatile((self.ecam_addr + offset as u64) as *const u32);

        // Check if I/O BAR (skip those)
        if (val & 0x1) != 0 {
            return 0;
        }

        let is_64bit = (val & 0x4) != 0;
        let mut addr = (val & 0xFFFFFFF0) as u64;

        if is_64bit && idx < 5 {
            let high = read_volatile((self.ecam_addr + offset as u64 + 4) as *const u32);
            addr |= (high as u64) << 32;
        }

        addr
    }

    /// Read all existing BAR addresses (what VZ programmed)
    pub unsafe fn read_bars(&mut self) {
        let mut bar_idx = 0;
        while bar_idx < 6 {
            let offset = PCI_BAR0 + (bar_idx * 4);
            let val = read_volatile((self.ecam_addr + offset as u64) as *const u32);

            // Check if I/O BAR (skip those)
            if (val & 0x1) != 0 {
                bar_idx += 1;
                continue;
            }

            let is_64bit = (val & 0x4) != 0;
            let mut addr = (val & 0xFFFFFFF0) as u64;

            if is_64bit && bar_idx < 5 {
                let high = read_volatile((self.ecam_addr + offset as u64 + 4) as *const u32);
                addr |= (high as u64) << 32;
            }

            self.bars[bar_idx] = addr;
            bar_idx += if is_64bit { 2 } else { 1 };
        }
    }

    /// Get the size of a BAR by probing (write all 1s, read back)
    pub unsafe fn get_bar_size(&self, bar_idx: usize) -> (u64, bool) {
        if bar_idx >= 6 {
            return (0, false);
        }

        let offset = PCI_BAR0 + (bar_idx * 4);
        let bar_ptr = (self.ecam_addr + offset as u64) as *mut u32;

        // Disable memory decode while probing
        let cmd_ptr = (self.ecam_addr + PCI_COMMAND as u64) as *mut u16;
        let orig_cmd = read_volatile(cmd_ptr);
        write_volatile(cmd_ptr, orig_cmd & !0x03);
        fence(Ordering::SeqCst);

        // Read original value
        let orig_val = read_volatile(bar_ptr);

        // Write all 1s
        write_volatile(bar_ptr, 0xFFFFFFFF);
        fence(Ordering::SeqCst);
        let mask = read_volatile(bar_ptr);

        // Restore original
        write_volatile(bar_ptr, orig_val);
        fence(Ordering::SeqCst);

        // Re-enable decode
        write_volatile(cmd_ptr, orig_cmd);
        fence(Ordering::SeqCst);

        // Check if BAR is implemented
        if mask == 0 || mask == 0xFFFFFFFF {
            return (0, false);
        }

        let is_64bit = (orig_val & 0x4) != 0;
        let size = (!(mask & 0xFFFFFFF0)).wrapping_add(1) as u64;

        (size, is_64bit)
    }

    /// Program a BAR with a new address
    pub unsafe fn program_bar(&mut self, bar_idx: usize, addr: u64) -> bool {
        if bar_idx >= 6 {
            return false;
        }

        let offset = PCI_BAR0 + (bar_idx * 4);
        let bar_ptr = (self.ecam_addr + offset as u64) as *mut u32;

        // Read to check if 64-bit
        let orig_val = read_volatile(bar_ptr);
        let is_64bit = (orig_val & 0x4) != 0;

        // Disable memory decode
        let cmd_ptr = (self.ecam_addr + PCI_COMMAND as u64) as *mut u16;
        let orig_cmd = read_volatile(cmd_ptr);
        write_volatile(cmd_ptr, orig_cmd & !0x03);
        fence(Ordering::SeqCst);

        // Write low 32 bits (preserve type bits)
        write_volatile(bar_ptr, (addr as u32) | (orig_val & 0xF));
        fence(Ordering::SeqCst);

        // Write high 32 bits for 64-bit BARs
        if is_64bit && bar_idx < 5 {
            write_volatile(
                (self.ecam_addr + offset as u64 + 4) as *mut u32,
                (addr >> 32) as u32,
            );
            fence(Ordering::SeqCst);
        }

        // Verify write was accepted
        let readback = read_volatile(bar_ptr);
        let readback_addr = (readback & 0xFFFFFFF0) as u64;
        let mut accepted = readback_addr == (addr as u32 & 0xFFFFFFF0) as u64;

        if is_64bit && accepted {
            let high = read_volatile((self.ecam_addr + offset as u64 + 4) as *const u32) as u64;
            accepted = high == (addr >> 32);
        }

        // Re-enable decode + bus master
        write_volatile(cmd_ptr, orig_cmd | 0x06);
        fence(Ordering::SeqCst);

        if accepted {
            self.bars[bar_idx] = addr;
        }

        accepted
    }

    /// Enable memory space and bus master
    pub unsafe fn enable(&self) {
        let cmd_ptr = (self.ecam_addr + PCI_COMMAND as u64) as *mut u16;
        let cmd = read_volatile(cmd_ptr);
        write_volatile(cmd_ptr, cmd | 0x06); // Memory + Bus Master
        fence(Ordering::SeqCst);
    }
}

// VirtIO PCI capability types
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// VirtIO Modern device with parsed capability locations
#[derive(Clone, Copy, Debug)]
pub struct VirtioModern {
    pub common: u64,
    pub notify: u64,
    pub isr: u64,
    pub device: u64,
    pub notify_mult: u32,
}

impl VirtioModern {
    /// Parse VirtIO capabilities from a PCI device
    /// Returns None if BAR is unmapped (address = 0)
    pub unsafe fn probe(dev: &PciDevice) -> Option<Self> {
        let mut common = 0u64;
        let mut notify = 0u64;
        let mut isr = 0u64;
        let mut device = 0u64;
        let mut notify_mult = 0u32;

        // Check if device has capability list
        let status = read_volatile((dev.ecam_addr + PCI_STATUS as u64) as *const u16);
        if (status & 0x10) == 0 {
            return None;
        }

        // Walk capability list
        let mut cap_offset = read_volatile((dev.ecam_addr + PCI_CAP_PTR as u64) as *const u8);

        while cap_offset != 0 && cap_offset != 0xFF {
            let cap_addr = dev.ecam_addr + cap_offset as u64;
            let cap_id = read_volatile(cap_addr as *const u8);
            let next_offset = read_volatile((cap_addr + 1) as *const u8);

            // VirtIO vendor capability (0x09)
            if cap_id == 0x09 {
                let cfg_type = read_volatile((cap_addr + 3) as *const u8);
                let bar_idx = read_volatile((cap_addr + 4) as *const u8);
                let offset = read_volatile((cap_addr + 8) as *const u32);

                if bar_idx < 6 {
                    let bar_addr = dev.bars[bar_idx as usize];

                    // CRITICAL: Only use if BAR is actually mapped
                    if bar_addr != 0 {
                        let final_addr = bar_addr + offset as u64;

                        match cfg_type {
                            VIRTIO_PCI_CAP_COMMON_CFG => common = final_addr,
                            VIRTIO_PCI_CAP_NOTIFY_CFG => {
                                notify = final_addr;
                                notify_mult = read_volatile((cap_addr + 16) as *const u32);
                            }
                            VIRTIO_PCI_CAP_ISR_CFG => isr = final_addr,
                            VIRTIO_PCI_CAP_DEVICE_CFG => device = final_addr,
                            _ => {}
                        }
                    }
                }
            }

            cap_offset = next_offset;
        }

        // Need at least common and notify to function
        if common != 0 && notify != 0 {
            Some(VirtioModern {
                common,
                notify,
                isr,
                device,
                notify_mult,
            })
        } else {
            None
        }
    }

    /// Initialize a VirtIO device (reset → ack → driver → features → OK)
    pub unsafe fn init_device(&self) -> bool {
        // 1. Reset device
        write_volatile((self.common + 20) as *mut u8, 0);
        fence(Ordering::SeqCst);

        // Wait for reset to complete
        for _ in 0..1000 {
            if read_volatile((self.common + 20) as *const u8) == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // 2. Acknowledge
        write_volatile((self.common + 20) as *mut u8, 0x01);
        fence(Ordering::SeqCst);

        // 3. Driver
        write_volatile((self.common + 20) as *mut u8, 0x03);
        fence(Ordering::SeqCst);

        // 4. Read features (bank 0)
        write_volatile((self.common + 0) as *mut u32, 0);
        fence(Ordering::SeqCst);
        let _features0 = read_volatile((self.common + 4) as *const u32);

        // 5. Accept NO features in bank 0
        write_volatile((self.common + 8) as *mut u32, 0);
        fence(Ordering::SeqCst);
        write_volatile((self.common + 12) as *mut u32, 0);
        fence(Ordering::SeqCst);

        // 6. Accept VERSION_1 in bank 1 (bit 32 = bit 0 of bank 1)
        write_volatile((self.common + 8) as *mut u32, 1);
        fence(Ordering::SeqCst);
        write_volatile((self.common + 12) as *mut u32, 1);
        fence(Ordering::SeqCst);

        // 7. Features OK
        write_volatile((self.common + 20) as *mut u8, 0x0B);
        fence(Ordering::SeqCst);

        // 8. Verify FEATURES_OK
        let status = read_volatile((self.common + 20) as *const u8);
        if (status & 0x08) == 0 {
            return false;
        }

        // 9. Driver OK
        write_volatile((self.common + 20) as *mut u8, 0x0F);
        fence(Ordering::SeqCst);

        true
    }
}

// Legacy functions for compatibility
pub unsafe fn find_ecam_from_dtb(dtb_ptr: u64) -> Option<u64> {
    crate::dtb::find_ecam_base(dtb_ptr)
}
