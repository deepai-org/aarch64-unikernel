//! Minimal PCI ECAM scanner for finding virtio devices
//!
//! VZ uses PCI-based virtio, not MMIO. We need to:
//! 1. Find PCI ECAM base from DTB (or try common addresses)
//! 2. Scan config space for virtio vendor (0x1af4)
//! 3. Map BAR0 to access virtio registers

use core::ptr::{read_volatile, write_volatile};

// Virtio vendor ID
const VIRTIO_VENDOR_ID: u16 = 0x1af4;
// Virtio console device ID (PCI subsystem)
const VIRTIO_CONSOLE_DEVICE_ID: u16 = 0x1003;

// Common PCI ECAM base addresses to try
// NOTE: 0x40000000 is RAM_BASE - do NOT include it!
// VZ on Apple Silicon may use high addresses (above 4GB)
const ECAM_BASES: &[u64] = &[
    0x3f000000,     // QEMU virt / VZ default
    0x30000000,
    0x50000000,
    0x80000000,
    0xb0000000,
    0xc0000000,
    0xe0000000,
    0xf0000000,
    0x10000000,
    0x20000000,
    // High addresses (above 4GB)
    0x1_00000000,
    0x2_00000000,
    0x4_00000000,
    0x5_00000000,
    0x6_00000000,
    0x8_00000000,
    0x10_00000000,
];

// PCI config space offsets
const PCI_VENDOR_ID: usize = 0x00;
const PCI_DEVICE_ID: usize = 0x02;
const PCI_COMMAND: usize = 0x04;
const PCI_BAR0: usize = 0x10;
const PCI_SUBSYS_ID: usize = 0x2e;

pub struct PciDevice {
    pub ecam_base: u64,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub bar0: u64,
}

impl PciDevice {
    /// Get config space address for this device
    fn config_addr(&self, offset: usize) -> u64 {
        self.ecam_base
            + ((self.bus as u64) << 20)
            + ((self.device as u64) << 15)
            + ((self.function as u64) << 12)
            + offset as u64
    }

    /// Read 16-bit value from config space
    pub fn read_config_u16(&self, offset: usize) -> u16 {
        unsafe { read_volatile(self.config_addr(offset) as *const u16) }
    }

    /// Read 32-bit value from config space
    pub fn read_config_u32(&self, offset: usize) -> u32 {
        unsafe { read_volatile(self.config_addr(offset) as *const u32) }
    }

    /// Write 16-bit value to config space
    pub fn write_config_u16(&self, offset: usize, value: u16) {
        unsafe { write_volatile(self.config_addr(offset) as *mut u16, value) }
    }

    /// Enable memory space access
    pub fn enable_memory(&self) {
        let cmd = self.read_config_u16(PCI_COMMAND);
        self.write_config_u16(PCI_COMMAND, cmd | 0x02);
    }

    /// Get BAR0 address (for virtio MMIO access)
    pub fn get_bar0(&self) -> u64 {
        let bar0_low = self.read_config_u32(PCI_BAR0);
        // Check if 64-bit BAR
        if (bar0_low & 0x6) == 0x4 {
            let bar0_high = self.read_config_u32(PCI_BAR0 + 4);
            ((bar0_high as u64) << 32) | ((bar0_low & !0xf) as u64)
        } else {
            (bar0_low & !0xf) as u64
        }
    }
}

/// Check if ECAM is valid at this address
fn probe_ecam(base: u64) -> bool {
    unsafe {
        // Try to read vendor ID at bus 0, device 0, function 0
        let vendor = read_volatile(base as *const u16);
        // Valid vendor IDs are non-zero and not 0xffff
        vendor != 0 && vendor != 0xffff
    }
}

/// Scan for PCI devices
pub fn scan_pci(ecam_base: u64) -> Option<PciDevice> {
    // Scan first few buses
    for bus in 0..4u8 {
        for device in 0..32u8 {
            let config_addr = ecam_base
                + ((bus as u64) << 20)
                + ((device as u64) << 15);

            unsafe {
                let vendor_id = read_volatile(config_addr as *const u16);
                if vendor_id == 0 || vendor_id == 0xffff {
                    continue;
                }

                let device_id = read_volatile((config_addr + 2) as *const u16);

                // Check for virtio vendor
                if vendor_id == VIRTIO_VENDOR_ID {
                    let bar0_low = read_volatile((config_addr + PCI_BAR0 as u64) as *const u32);
                    let bar0 = if (bar0_low & 0x6) == 0x4 {
                        let bar0_high = read_volatile((config_addr + PCI_BAR0 as u64 + 4) as *const u32);
                        ((bar0_high as u64) << 32) | ((bar0_low & !0xf) as u64)
                    } else {
                        (bar0_low & !0xf) as u64
                    };

                    return Some(PciDevice {
                        ecam_base,
                        bus,
                        device,
                        function: 0,
                        vendor_id,
                        device_id,
                        bar0,
                    });
                }
            }
        }
    }
    None
}

/// Find virtio device by probing ECAM bases
pub fn find_virtio_pci() -> Option<PciDevice> {
    for &base in ECAM_BASES {
        if probe_ecam(base) {
            if let Some(dev) = scan_pci(base) {
                return Some(dev);
            }
        }
    }
    None
}

/// Try to find PCI ECAM from DTB
pub unsafe fn find_ecam_from_dtb(dtb_ptr: u64) -> Option<u64> {
    if dtb_ptr == 0 {
        return None;
    }

    let magic = read_volatile(dtb_ptr as *const u32).swap_bytes();
    if magic != 0xd00dfeed {
        return None;
    }

    let off_dt_struct = read_volatile((dtb_ptr + 8) as *const u32).swap_bytes() as u64;
    let off_dt_strings = read_volatile((dtb_ptr + 12) as *const u32).swap_bytes() as u64;
    let total_size = read_volatile((dtb_ptr + 4) as *const u32).swap_bytes() as u64;

    let struct_base = dtb_ptr + off_dt_struct;
    let strings_base = dtb_ptr + off_dt_strings;
    let max_pos = total_size.saturating_sub(off_dt_struct);

    let mut pos: u64 = 0;
    let mut in_pcie_node = false;

    while pos < max_pos {
        let token = read_volatile((struct_base + pos) as *const u32).swap_bytes();
        pos += 4;

        match token {
            0x1 => {
                // FDT_BEGIN_NODE
                let name_start = struct_base + pos;
                let mut name_len = 0u64;
                while name_len < 256 {
                    let c = read_volatile((name_start + name_len) as *const u8);
                    if c == 0 { break; }
                    name_len += 1;
                }

                // Check if this is a pcie node
                let name_slice = core::slice::from_raw_parts(name_start as *const u8, name_len as usize);
                if let Ok(name) = core::str::from_utf8(name_slice) {
                    if name.starts_with("pcie") || name.starts_with("pci") {
                        in_pcie_node = true;
                    }
                }

                pos += (name_len + 4) & !3;
            }
            0x2 => {
                // FDT_END_NODE
                in_pcie_node = false;
            }
            0x3 => {
                // FDT_PROP
                let len = read_volatile((struct_base + pos) as *const u32).swap_bytes();
                let nameoff = read_volatile((struct_base + pos + 4) as *const u32).swap_bytes();
                pos += 8;

                if in_pcie_node {
                    // Check property name
                    let prop_name_ptr = strings_base + nameoff as u64;
                    let mut prop_name_len = 0;
                    while prop_name_len < 32 {
                        let c = read_volatile((prop_name_ptr + prop_name_len) as *const u8);
                        if c == 0 { break; }
                        prop_name_len += 1;
                    }
                    let prop_slice = core::slice::from_raw_parts(prop_name_ptr as *const u8, prop_name_len as usize);
                    if let Ok(prop_name) = core::str::from_utf8(prop_slice) {
                        if prop_name == "reg" && len >= 8 {
                            // First reg entry is usually ECAM base
                            let addr_hi = read_volatile((struct_base + pos) as *const u32).swap_bytes() as u64;
                            let addr_lo = read_volatile((struct_base + pos + 4) as *const u32).swap_bytes() as u64;
                            let addr = (addr_hi << 32) | addr_lo;
                            if addr != 0 {
                                return Some(addr);
                            }
                        }
                    }
                }

                pos += ((len as u64) + 3) & !3;
            }
            0x9 => break,
            _ => {}
        }
    }

    None
}
