//! Modern PCI enumeration with BAR probing and capability parsing
//! This implements Linux-like PCI resource allocation

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

const PCI_COMMAND: u64 = 0x04;
const PCI_STATUS: u64 = 0x06;
const PCI_CAP_PTR: u64 = 0x34;
const PCI_STATUS_CAP_LIST: u16 = 1 << 4;

// Global allocator for unmapped BARs
// Use 0x5100_0000 which is:
// 1. Inside the PCI MMIO window (0x5000_0000 - 0x6FFF_FFFF)
// 2. Above VZ's pre-mapped devices (Console=0x5000C000, GPU=0x50008000)
// NOTE: 0x8000_0000 was WRONG - it's inside RAM (0x7000_0000-0xAFFF_FFFF)!
static mut MMIO_ALLOC_PTR: u64 = 0x5100_0000;

/// PCI device with probed and allocated BARs
pub struct PciDevice {
    pub ecam_addr: u64,
    pub vendor_id: u16,
    pub device_id: u16,
    pub bars: [u64; 6],
}

impl PciDevice {
    /// Create and initialize a PCI device at the given location
    pub unsafe fn new(ecam_base: u64, slot: u8) -> Option<Self> {
        let addr = ecam_base + ((slot as u64) << 15);
        let vendor = read_volatile(addr as *const u16);

        if vendor == 0xFFFF || vendor == 0 {
            return None;
        }

        let device = read_volatile((addr + 2) as *const u16);

        let mut dev = PciDevice {
            ecam_addr: addr,
            vendor_id: vendor,
            device_id: device,
            bars: [0; 6],
        };

        dev.probe_and_allocate_bars();
        Some(dev)
    }

    /// Probe BAR sizes and allocate addresses for unmapped BARs
    unsafe fn probe_and_allocate_bars(&mut self) {
        // 1. Disable decoding while we mess with BARs
        let cmd_ptr = (self.ecam_addr + PCI_COMMAND) as *mut u16;
        let orig_cmd = read_volatile(cmd_ptr);
        write_volatile(cmd_ptr, orig_cmd & !0x03);
        fence(Ordering::SeqCst);

        let mut i = 0;
        while i < 6 {
            let bar_offset = 0x10 + (i as u64 * 4);
            let bar_ptr = (self.ecam_addr + bar_offset) as *mut u32;

            // Read original value
            let orig_val = read_volatile(bar_ptr);

            // Check if IO space (bit 0)
            let is_io = (orig_val & 0x1) != 0;
            if is_io {
                i += 1;
                continue;
            }

            // Check if 64-bit (bits 2:1 == 0b10)
            let is_64 = (orig_val & 0x6) == 0x4;

            // 2. Probe Size: Write all 1s
            write_volatile(bar_ptr, 0xFFFFFFFF);
            fence(Ordering::SeqCst);
            let size_mask = read_volatile(bar_ptr);

            // Restore original
            write_volatile(bar_ptr, orig_val);
            fence(Ordering::SeqCst);

            if size_mask == 0 || size_mask == 0xFFFFFFFF {
                i += 1;
                continue;
            }

            // Calculate size (mask out type bits)
            let size = (!(size_mask & 0xFFFFFFF0)).wrapping_add(1);
            if size == 0 {
                i += 1;
                continue;
            }

            // 3. Get current address or allocate new one
            let mut final_addr = (orig_val & 0xFFFFFFF0) as u64;

            if is_64 {
                // Read high 32 bits
                let bar1_ptr = (self.ecam_addr + bar_offset + 4) as *const u32;
                let high = read_volatile(bar1_ptr) as u64;
                final_addr |= high << 32;
            }

            if final_addr == 0 {
                // BAR is unmapped - allocate from our pool
                let align_mask = (size as u64).saturating_sub(1);
                MMIO_ALLOC_PTR = (MMIO_ALLOC_PTR + align_mask) & !align_mask;

                final_addr = MMIO_ALLOC_PTR;
                MMIO_ALLOC_PTR += size as u64;

                // Write new address (disable decode first per PCI spec)
                let cmd_save = read_volatile(cmd_ptr);
                write_volatile(cmd_ptr, cmd_save & !0x02);
                fence(Ordering::SeqCst);

                write_volatile(bar_ptr, (final_addr as u32) | (orig_val & 0xF));
                fence(Ordering::SeqCst);

                if is_64 {
                    let bar1_ptr = (self.ecam_addr + bar_offset + 4) as *mut u32;
                    write_volatile(bar1_ptr, (final_addr >> 32) as u32);
                    fence(Ordering::SeqCst);
                }

                // Re-enable decode
                write_volatile(cmd_ptr, cmd_save);
                fence(Ordering::SeqCst);

                // Check if write was accepted
                let readback = read_volatile(bar_ptr);
                let readback_addr = (readback & 0xFFFFFFF0) as u64;
                let mut accepted = readback_addr == (final_addr as u32 & 0xFFFFFFF0) as u64;

                if is_64 && accepted {
                    let high = read_volatile((self.ecam_addr + bar_offset + 4) as *const u32) as u64;
                    accepted = high == (final_addr >> 32);
                }

                if !accepted {
                    // VZ rejected our BAR write - revert final_addr to 0
                    // The device might still work at a Ghost Map address
                    final_addr = 0;
                }
            }

            self.bars[i] = final_addr;

            // Skip next BAR index for 64-bit BARs
            if is_64 {
                i += 2;
            } else {
                i += 1;
            }
        }

        // 4. Enable Bus Master and Memory Space
        write_volatile(cmd_ptr, orig_cmd | 0x06);
        fence(Ordering::SeqCst);
    }

    /// Find a PCI capability by ID, returns offset in config space
    pub unsafe fn find_capability(&self, cap_id: u8) -> Option<u8> {
        let status = read_volatile((self.ecam_addr + PCI_STATUS) as *const u16);
        if (status & PCI_STATUS_CAP_LIST) == 0 {
            return None;
        }

        let mut offset = read_volatile((self.ecam_addr + PCI_CAP_PTR) as *const u8);
        while offset != 0 && offset != 0xFF {
            let this_id = read_volatile((self.ecam_addr + offset as u64) as *const u8);
            if this_id == cap_id {
                return Some(offset);
            }
            offset = read_volatile((self.ecam_addr + offset as u64 + 1) as *const u8);
        }
        None
    }

    /// Iterate all capabilities of a given type
    pub unsafe fn iter_capabilities(&self, cap_id: u8) -> CapabilityIter {
        CapabilityIter {
            ecam_addr: self.ecam_addr,
            cap_id,
            next_offset: if self.find_capability(cap_id).is_some() {
                read_volatile((self.ecam_addr + PCI_CAP_PTR) as *const u8)
            } else {
                0
            },
        }
    }
}

/// Iterator over PCI capabilities
pub struct CapabilityIter {
    ecam_addr: u64,
    cap_id: u8,
    next_offset: u8,
}

impl Iterator for CapabilityIter {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        unsafe {
            while self.next_offset != 0 && self.next_offset != 0xFF {
                let offset = self.next_offset;
                let this_id = read_volatile((self.ecam_addr + offset as u64) as *const u8);
                self.next_offset = read_volatile((self.ecam_addr + offset as u64 + 1) as *const u8);

                if this_id == self.cap_id {
                    return Some(offset);
                }
            }
            None
        }
    }
}

/// VirtIO capability types
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
pub const VIRTIO_PCI_CAP_PCI_CFG: u8 = 5;

/// Parsed VirtIO capability
#[derive(Clone, Copy, Default)]
pub struct VirtioCap {
    pub cfg_type: u8,
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
    pub notify_off_multiplier: u32, // Only for NOTIFY_CFG
}

/// Parse a VirtIO capability at the given config space offset
pub unsafe fn parse_virtio_cap(ecam_addr: u64, cap_offset: u8) -> VirtioCap {
    let addr = ecam_addr + cap_offset as u64;

    let cfg_type = read_volatile((addr + 3) as *const u8);
    let bar = read_volatile((addr + 4) as *const u8);
    let offset = read_volatile((addr + 8) as *const u32);
    let length = read_volatile((addr + 12) as *const u32);

    let notify_off_multiplier = if cfg_type == VIRTIO_PCI_CAP_NOTIFY_CFG {
        read_volatile((addr + 16) as *const u32)
    } else {
        0
    };

    VirtioCap {
        cfg_type,
        bar,
        offset,
        length,
        notify_off_multiplier,
    }
}

/// Modern VirtIO device with parsed capability locations
pub struct VirtioModernDevice {
    pub common_cfg: u64,      // Absolute MMIO address of common config
    pub notify_cfg: u64,      // Absolute MMIO address of notify region
    pub notify_off_multiplier: u32,
    pub isr_cfg: u64,
    pub device_cfg: u64,
    pub device_id: u16,
}

/// Ghost Map: fallback addresses when VZ rejects BAR programming
/// These are used if our dynamic allocation fails - places devices in the
/// PCI MMIO window (0x51XX_XXXX) above VZ's reserved region
fn ghost_map_bar0(device_id: u16) -> u64 {
    // VZ pre-programs some devices (Console, GPU) but not others (Network, Balloon)
    // For devices VZ programs, keep their known addresses (0x5000_XXXX)
    // For devices VZ doesn't program, use our allocation region (0x51XX_XXXX)
    match device_id {
        0x1041 => 0x5110_0000, // Network - in our MMIO region
        0x1042 => 0x5000_4000, // Block - VZ usually programs this
        0x1043 => 0x5000_C000, // Console - VZ programs this
        0x1044 => 0x5001_0000, // Entropy - VZ sometimes programs
        0x1045 => 0x5120_0000, // Balloon - in our MMIO region
        0x1050 => 0x5000_8000, // GPU - VZ programs this
        _ => 0,
    }
}

impl VirtioModernDevice {
    /// Parse VirtIO capabilities and create a modern device
    pub unsafe fn new(pci: &PciDevice) -> Option<Self> {
        let mut common = 0u64;
        let mut notify = 0u64;
        let mut notify_mult = 0u32;
        let mut isr = 0u64;
        let mut device = 0u64;

        // Walk capability list looking for VirtIO vendor caps (0x09)
        let status = read_volatile((pci.ecam_addr + PCI_STATUS) as *const u16);
        if (status & PCI_STATUS_CAP_LIST) == 0 {
            return None;
        }

        let mut offset = read_volatile((pci.ecam_addr + PCI_CAP_PTR) as *const u8);

        while offset != 0 && offset != 0xFF {
            let cap_id = read_volatile((pci.ecam_addr + offset as u64) as *const u8);

            if cap_id == 0x09 {
                // VirtIO vendor capability
                let cap = parse_virtio_cap(pci.ecam_addr, offset);

                if cap.bar < 6 {
                    // Get BAR address, with Ghost Map fallback
                    let mut bar_addr = pci.bars[cap.bar as usize];
                    if bar_addr == 0 && cap.bar == 0 {
                        // BAR0 is unmapped - try Ghost Map
                        bar_addr = ghost_map_bar0(pci.device_id);
                    }

                    if bar_addr != 0 {
                        let abs_addr = bar_addr + cap.offset as u64;

                        match cap.cfg_type {
                            VIRTIO_PCI_CAP_COMMON_CFG => common = abs_addr,
                            VIRTIO_PCI_CAP_NOTIFY_CFG => {
                                notify = abs_addr;
                                notify_mult = cap.notify_off_multiplier;
                            }
                            VIRTIO_PCI_CAP_ISR_CFG => isr = abs_addr,
                            VIRTIO_PCI_CAP_DEVICE_CFG => device = abs_addr,
                            _ => {}
                        }
                    }
                }
            }

            offset = read_volatile((pci.ecam_addr + offset as u64 + 1) as *const u8);
        }

        if common == 0 {
            return None;
        }

        Some(VirtioModernDevice {
            common_cfg: common,
            notify_cfg: notify,
            notify_off_multiplier: notify_mult,
            isr_cfg: isr,
            device_cfg: device,
            device_id: pci.device_id,
        })
    }

    /// Reset the device
    pub unsafe fn reset(&self) {
        // device_status is at offset 20 in common config
        write_volatile((self.common_cfg + 20) as *mut u8, 0);
        fence(Ordering::SeqCst);
    }

    /// Get device status
    pub unsafe fn status(&self) -> u8 {
        read_volatile((self.common_cfg + 20) as *const u8)
    }

    /// Set device status
    pub unsafe fn set_status(&self, status: u8) {
        write_volatile((self.common_cfg + 20) as *mut u8, status);
        fence(Ordering::SeqCst);
    }
}
