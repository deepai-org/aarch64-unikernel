//! VirtIO Entropy (RNG) driver
//!
//! The simplest VirtIO device - just one queue, device fills buffers with random bytes.
//! Device IDs: 0x1004 (transitional), 0x1044 (modern)

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// Virtio vendor ID
const VIRTIO_VENDOR_ID: u16 = 0x1af4;

// Device IDs for entropy device
const VIRTIO_ENTROPY_TRANSITIONAL: u16 = 0x1004;
const VIRTIO_ENTROPY_MODERN: u16 = 0x1044;

// PCI config space offsets
const PCI_COMMAND: usize = 0x04;
const PCI_STATUS: usize = 0x06;
const PCI_BAR0: usize = 0x10;
const PCI_CAP_PTR: usize = 0x34;

// PCI command bits
const PCI_COMMAND_MEMORY: u16 = 0x02;
const PCI_COMMAND_BUS_MASTER: u16 = 0x04;

// VirtIO PCI capability types
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;

// Virtio device status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;

// Common configuration offsets
const VIRTIO_PCI_COMMON_DFSELECT: usize = 0x00;
const VIRTIO_PCI_COMMON_DF: usize = 0x04;
const VIRTIO_PCI_COMMON_GFSELECT: usize = 0x08;
const VIRTIO_PCI_COMMON_GF: usize = 0x0c;
const VIRTIO_PCI_COMMON_STATUS: usize = 0x14;
const VIRTIO_PCI_COMMON_Q_SELECT: usize = 0x16;
const VIRTIO_PCI_COMMON_Q_SIZE: usize = 0x18;
const VIRTIO_PCI_COMMON_Q_ENABLE: usize = 0x1c;
const VIRTIO_PCI_COMMON_Q_NOFF: usize = 0x1e;
const VIRTIO_PCI_COMMON_Q_DESCLO: usize = 0x20;
const VIRTIO_PCI_COMMON_Q_DESCHI: usize = 0x24;
const VIRTIO_PCI_COMMON_Q_AVAILLO: usize = 0x28;
const VIRTIO_PCI_COMMON_Q_AVAILHI: usize = 0x2c;
const VIRTIO_PCI_COMMON_Q_USEDLO: usize = 0x30;
const VIRTIO_PCI_COMMON_Q_USEDHI: usize = 0x34;

// Descriptor flag for device-writable buffer
const VRING_DESC_F_WRITE: u16 = 2;

const QUEUE_SIZE: u16 = 4;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VringDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct VringAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE as usize],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VringUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct VringUsed {
    flags: u16,
    idx: u16,
    ring: [VringUsedElem; QUEUE_SIZE as usize],
}

// Static buffers for the entropy queue
static mut ENTROPY_QUEUE_DESCS: [VringDesc; QUEUE_SIZE as usize] =
    [VringDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE as usize];
static mut ENTROPY_QUEUE_AVAIL: VringAvail = VringAvail {
    flags: 0, idx: 0, ring: [0; QUEUE_SIZE as usize]
};
static mut ENTROPY_QUEUE_USED: VringUsed = VringUsed {
    flags: 0, idx: 0, ring: [VringUsedElem { id: 0, len: 0 }; QUEUE_SIZE as usize]
};

// Buffer to receive random bytes
static mut ENTROPY_BUFFER: [u8; 64] = [0; 64];
static mut ENTROPY_IDX: u16 = 0;
static mut ENTROPY_LAST_USED: u16 = 0;

/// VirtIO Entropy device capability
#[derive(Clone, Copy)]
struct VirtioCap {
    bar: u8,
    offset: u32,
    notify_off_multiplier: u32,
}

/// VirtIO Entropy driver
pub struct VirtioEntropy {
    bar0: u64,
    common_offset: u32,
    notify_offset: u32,
    notify_multiplier: u32,
    queue_notify_off: u16,
}

impl VirtioEntropy {
    /// Create from pre-configured VirtioModern transport
    pub unsafe fn from_modern(modern: &crate::pci::VirtioModern, _ecam_addr: u64) -> Option<Self> {
        use core::sync::atomic::{fence, Ordering};

        let common_base = modern.common;
        let notify_base = modern.notify;

        // Reset device
        write_volatile((common_base + 20) as *mut u8, 0);
        fence(Ordering::SeqCst);
        for _ in 0..1000 {
            if read_volatile((common_base + 20) as *const u8) == 0 { break; }
            core::hint::spin_loop();
        }

        // Acknowledge + Driver
        write_volatile((common_base + 20) as *mut u8, 0x03);
        fence(Ordering::SeqCst);

        // Accept VERSION_1 only
        write_volatile((common_base + 8) as *mut u32, 0);
        write_volatile((common_base + 12) as *mut u32, 0);
        write_volatile((common_base + 8) as *mut u32, 1);
        write_volatile((common_base + 12) as *mut u32, 1);
        fence(Ordering::SeqCst);

        // Features OK
        write_volatile((common_base + 20) as *mut u8, 0x0B);
        fence(Ordering::SeqCst);

        if (read_volatile((common_base + 20) as *const u8) & 0x08) == 0 {
            return None;
        }

        // Setup queue 0
        write_volatile((common_base + 22) as *mut u16, 0);
        fence(Ordering::SeqCst);

        let queue_size_max = read_volatile((common_base + 24) as *const u16);
        if queue_size_max == 0 { return None; }

        let actual_size = queue_size_max.min(QUEUE_SIZE);
        write_volatile((common_base + 24) as *mut u16, actual_size);

        let desc_addr = &raw const ENTROPY_QUEUE_DESCS as u64;
        let avail_addr = &raw const ENTROPY_QUEUE_AVAIL as u64;
        let used_addr = &raw const ENTROPY_QUEUE_USED as u64;

        write_volatile((common_base + 32) as *mut u32, desc_addr as u32);
        write_volatile((common_base + 36) as *mut u32, (desc_addr >> 32) as u32);
        write_volatile((common_base + 40) as *mut u32, avail_addr as u32);
        write_volatile((common_base + 44) as *mut u32, (avail_addr >> 32) as u32);
        write_volatile((common_base + 48) as *mut u32, used_addr as u32);
        write_volatile((common_base + 52) as *mut u32, (used_addr >> 32) as u32);

        let queue_notify_off = read_volatile((common_base + 30) as *const u16);

        write_volatile((common_base + 28) as *mut u16, 1);
        fence(Ordering::SeqCst);

        // Driver OK
        write_volatile((common_base + 20) as *mut u8, 0x0F);
        fence(Ordering::SeqCst);

        Some(VirtioEntropy {
            bar0: notify_base, // Use notify base directly
            common_offset: 0,
            notify_offset: 0,
            notify_multiplier: modern.notify_mult,
            queue_notify_off,
        })
    }

    /// Find and initialize entropy device on PCI bus (legacy)
    pub fn find(ecam: u64) -> Option<Self> {
        // Scan for entropy device
        for dev in 0u8..32 {
            let config_base = ecam + ((dev as u64) << 15);

            unsafe {
                let vendor = read_volatile(config_base as *const u16);
                if vendor != VIRTIO_VENDOR_ID {
                    continue;
                }

                let device_id = read_volatile((config_base + 2) as *const u16);
                if device_id != VIRTIO_ENTROPY_TRANSITIONAL && device_id != VIRTIO_ENTROPY_MODERN {
                    continue;
                }

                // Found entropy device, try to init
                if let Some(entropy) = Self::try_new(config_base, dev) {
                    return Some(entropy);
                }
            }
        }
        None
    }

    fn try_new(config_base: u64, _slot: u8) -> Option<Self> {
        unsafe {
            // Enable memory access and bus mastering first
            let cmd = read_volatile((config_base + PCI_COMMAND as u64) as *const u16);
            write_volatile(
                (config_base + PCI_COMMAND as u64) as *mut u16,
                cmd | PCI_COMMAND_MEMORY | PCI_COMMAND_BUS_MASTER,
            );
            fence(Ordering::SeqCst);

            // Get device ID for Ghost Map lookup
            let device_id = read_volatile((config_base + 2) as *const u16);

            // Use Ghost Map to get BAR0 - VZ ignores guest BAR writes but uses predictable addresses
            let bar0 = crate::get_virtio_bar0(config_base, device_id);

            if bar0 == 0 {
                return None;
            }

            // Parse capabilities
            let status = read_volatile((config_base + PCI_STATUS as u64) as *const u16);
            if (status & 0x10) == 0 {
                // No capabilities - this is a problem
                return None;
            }

            let mut cap_ptr = read_volatile((config_base + PCI_CAP_PTR as u64) as *const u8);
            let mut common_cap: Option<VirtioCap> = None;
            let mut notify_cap: Option<VirtioCap> = None;

            while cap_ptr != 0 {
                let cap_id = read_volatile((config_base + cap_ptr as u64) as *const u8);
                let cap_next = read_volatile((config_base + cap_ptr as u64 + 1) as *const u8);

                if cap_id == 0x09 {
                    let cfg_type = read_volatile((config_base + cap_ptr as u64 + 3) as *const u8);
                    let bar = read_volatile((config_base + cap_ptr as u64 + 4) as *const u8);
                    let offset = read_volatile((config_base + cap_ptr as u64 + 8) as *const u32);

                    let cap = VirtioCap {
                        bar,
                        offset,
                        notify_off_multiplier: if cfg_type == VIRTIO_PCI_CAP_NOTIFY_CFG {
                            read_volatile((config_base + cap_ptr as u64 + 16) as *const u32)
                        } else {
                            0
                        },
                    };

                    match cfg_type {
                        VIRTIO_PCI_CAP_COMMON_CFG => common_cap = Some(cap),
                        VIRTIO_PCI_CAP_NOTIFY_CFG => notify_cap = Some(cap),
                        _ => {}
                    }
                }

                cap_ptr = cap_next;
            }

            let common = common_cap?;
            let notify = notify_cap?;

            if common.bar != 0 || notify.bar != 0 {
                return None;
            }

            let common_base = bar0 + common.offset as u64;

            // Initialize device
            // 1. Reset
            write_volatile((common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8, 0);
            fence(Ordering::SeqCst);

            // 2. Acknowledge
            write_volatile(
                (common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE,
            );
            fence(Ordering::SeqCst);

            // 3. Driver
            write_volatile(
                (common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
            );
            fence(Ordering::SeqCst);

            // 4. Read features (entropy device has none)
            write_volatile((common_base + VIRTIO_PCI_COMMON_DFSELECT as u64) as *mut u32, 0);
            let _features = read_volatile((common_base + VIRTIO_PCI_COMMON_DF as u64) as *const u32);

            // 5. Accept no features, but MUST set VIRTIO_F_VERSION_1
            write_volatile((common_base + VIRTIO_PCI_COMMON_GFSELECT as u64) as *mut u32, 0);
            write_volatile((common_base + VIRTIO_PCI_COMMON_GF as u64) as *mut u32, 0);
            // Bank 1: set VIRTIO_F_VERSION_1 (bit 32 = bit 0 of bank 1)
            write_volatile((common_base + VIRTIO_PCI_COMMON_GFSELECT as u64) as *mut u32, 1);
            write_volatile((common_base + VIRTIO_PCI_COMMON_GF as u64) as *mut u32, 1);

            // 6. Features OK
            write_volatile(
                (common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
            );
            fence(Ordering::SeqCst);

            // Verify
            let status = read_volatile((common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *const u8);
            if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
                return None;
            }

            // 7. Setup queue 0 (requestq)
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SELECT as u64) as *mut u16, 0);
            fence(Ordering::SeqCst);

            let queue_size_max = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *const u16);
            if queue_size_max == 0 {
                return None;
            }

            let actual_size = queue_size_max.min(QUEUE_SIZE);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *mut u16, actual_size);

            // Set queue addresses
            let desc_addr = &raw const ENTROPY_QUEUE_DESCS as u64;
            let avail_addr = &raw const ENTROPY_QUEUE_AVAIL as u64;
            let used_addr = &raw const ENTROPY_QUEUE_USED as u64;

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCLO as u64) as *mut u32, desc_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCHI as u64) as *mut u32, (desc_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILLO as u64) as *mut u32, avail_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILHI as u64) as *mut u32, (avail_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDLO as u64) as *mut u32, used_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDHI as u64) as *mut u32, (used_addr >> 32) as u32);

            let queue_notify_off = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_NOFF as u64) as *const u16);

            // Enable queue
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_ENABLE as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            // 8. Driver OK
            write_volatile(
                (common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK,
            );
            fence(Ordering::SeqCst);

            Some(VirtioEntropy {
                bar0,
                common_offset: common.offset,
                notify_offset: notify.offset,
                notify_multiplier: notify.notify_off_multiplier,
                queue_notify_off,
            })
        }
    }

    /// Read random bytes from the device
    pub fn read(&self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }

        unsafe {
            let len = buf.len().min(ENTROPY_BUFFER.len());

            // Clear buffer
            for i in 0..len {
                ENTROPY_BUFFER[i] = 0;
            }

            let idx = ENTROPY_IDX % QUEUE_SIZE;

            // Set up descriptor - device-writable buffer
            ENTROPY_QUEUE_DESCS[idx as usize] = VringDesc {
                addr: ENTROPY_BUFFER.as_ptr() as u64,
                len: len as u32,
                flags: VRING_DESC_F_WRITE,
                next: 0,
            };

            fence(Ordering::SeqCst);

            // Add to available ring
            let avail_idx = read_volatile(&ENTROPY_QUEUE_AVAIL.idx);
            write_volatile(
                &raw mut ENTROPY_QUEUE_AVAIL.ring[(avail_idx % QUEUE_SIZE) as usize],
                idx,
            );
            fence(Ordering::SeqCst);
            write_volatile(&raw mut ENTROPY_QUEUE_AVAIL.idx, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Notify device
            let notify_addr = self.bar0
                + self.notify_offset as u64
                + (self.queue_notify_off as u64 * self.notify_multiplier as u64);
            write_volatile(notify_addr as *mut u16, 0);
            fence(Ordering::SeqCst);

            ENTROPY_IDX = ENTROPY_IDX.wrapping_add(1);

            // Wait for completion
            let mut bytes_received = 0usize;
            for _ in 0..100_000 {
                fence(Ordering::SeqCst);
                let used_idx = read_volatile(&ENTROPY_QUEUE_USED.idx);
                if used_idx != ENTROPY_LAST_USED {
                    // Get length from used ring
                    let used_elem = read_volatile(
                        &ENTROPY_QUEUE_USED.ring[(ENTROPY_LAST_USED % QUEUE_SIZE) as usize]
                    );
                    bytes_received = used_elem.len as usize;
                    ENTROPY_LAST_USED = used_idx;
                    break;
                }
                core::hint::spin_loop();
            }

            // Copy received bytes
            let copy_len = bytes_received.min(buf.len());
            for i in 0..copy_len {
                buf[i] = read_volatile(&ENTROPY_BUFFER[i]);
            }

            copy_len
        }
    }

    /// Test entropy quality - returns stats about randomness
    pub fn test_entropy(&self) -> EntropyStats {
        let mut buf = [0u8; 64];
        let bytes_read = self.read(&mut buf);

        if bytes_read == 0 {
            return EntropyStats {
                bytes_read: 0,
                zeros: 0,
                ones: 0,
                unique_bytes: 0,
                looks_random: false,
            };
        }

        // Count zeros and ones (bit distribution)
        let mut zeros = 0u32;
        let mut ones = 0u32;

        // Track unique byte values
        let mut seen = [false; 256];
        let mut unique = 0u32;

        for i in 0..bytes_read {
            let b = buf[i];

            // Count bits
            for bit in 0..8 {
                if (b >> bit) & 1 == 0 {
                    zeros += 1;
                } else {
                    ones += 1;
                }
            }

            // Track unique values
            if !seen[b as usize] {
                seen[b as usize] = true;
                unique += 1;
            }
        }

        // Good entropy should have ~50% zeros and ones, and many unique values
        let total_bits = (bytes_read * 8) as u32;
        let balance_ratio = if zeros > ones {
            (ones * 100) / zeros
        } else {
            (zeros * 100) / ones
        };

        // Consider it random if:
        // - Got some bytes
        // - Bit balance is at least 40% (reasonably balanced)
        // - At least 50% of bytes are unique (for 64 bytes, want at least 32 unique)
        let looks_random = bytes_read > 0
            && balance_ratio >= 40
            && unique >= (bytes_read as u32 / 2);

        EntropyStats {
            bytes_read,
            zeros,
            ones,
            unique_bytes: unique,
            looks_random,
        }
    }
}

/// Statistics about entropy quality
pub struct EntropyStats {
    pub bytes_read: usize,
    pub zeros: u32,
    pub ones: u32,
    pub unique_bytes: u32,
    pub looks_random: bool,
}
