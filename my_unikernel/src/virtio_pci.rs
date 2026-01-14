//! Virtio PCI driver for console output
//!
//! Virtio-PCI uses a completely different register layout than virtio-MMIO:
//! - Device discovery via PCI config space
//! - Capabilities in PCI capability list point to config structures
//! - Registers are in BAR0 at offsets specified by capabilities

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// PCI config space offsets
const PCI_VENDOR_ID: usize = 0x00;
const PCI_DEVICE_ID: usize = 0x02;
const PCI_COMMAND: usize = 0x04;
const PCI_STATUS: usize = 0x06;
const PCI_BAR0: usize = 0x10;
const PCI_CAP_PTR: usize = 0x34;

// PCI command bits
const PCI_COMMAND_MEMORY: u16 = 0x02;
const PCI_COMMAND_BUS_MASTER: u16 = 0x04;

// Virtio vendor ID
const VIRTIO_VENDOR_ID: u16 = 0x1af4;

// Virtio PCI device IDs (transitional)
const VIRTIO_PCI_CONSOLE: u16 = 0x1003;

// Virtio PCI capability types
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

// Virtio device status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;

// Common configuration offsets (within the common cfg structure)
const VIRTIO_PCI_COMMON_DFSELECT: usize = 0x00;
const VIRTIO_PCI_COMMON_DF: usize = 0x04;
const VIRTIO_PCI_COMMON_GFSELECT: usize = 0x08;
const VIRTIO_PCI_COMMON_GF: usize = 0x0c;
const VIRTIO_PCI_COMMON_MSIX: usize = 0x10;
const VIRTIO_PCI_COMMON_NUMQ: usize = 0x12;
const VIRTIO_PCI_COMMON_STATUS: usize = 0x14;
const VIRTIO_PCI_COMMON_CFGGEN: usize = 0x15;
const VIRTIO_PCI_COMMON_Q_SELECT: usize = 0x16;
const VIRTIO_PCI_COMMON_Q_SIZE: usize = 0x18;
const VIRTIO_PCI_COMMON_Q_MSIX: usize = 0x1a;
const VIRTIO_PCI_COMMON_Q_ENABLE: usize = 0x1c;
const VIRTIO_PCI_COMMON_Q_NOFF: usize = 0x1e;
const VIRTIO_PCI_COMMON_Q_DESCLO: usize = 0x20;
const VIRTIO_PCI_COMMON_Q_DESCHI: usize = 0x24;
const VIRTIO_PCI_COMMON_Q_AVAILLO: usize = 0x28;
const VIRTIO_PCI_COMMON_Q_AVAILHI: usize = 0x2c;
const VIRTIO_PCI_COMMON_Q_USEDLO: usize = 0x30;
const VIRTIO_PCI_COMMON_Q_USEDHI: usize = 0x34;

// Queue size
const QUEUE_SIZE: u16 = 16;

// Vring structures (same as MMIO)
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

#[repr(C, align(4096))]
struct VirtqueueBuffers {
    descs: [VringDesc; QUEUE_SIZE as usize],
    avail: VringAvail,
    _pad: [u8; 4096 - core::mem::size_of::<[VringDesc; QUEUE_SIZE as usize]>() - core::mem::size_of::<VringAvail>()],
    used: VringUsed,
}

#[repr(C, align(16))]
struct TxBuffer {
    data: [u8; 256],
}

// Static buffers
static mut PCI_TX_QUEUE: VirtqueueBuffers = VirtqueueBuffers {
    descs: [VringDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE as usize],
    avail: VringAvail { flags: 0, idx: 0, ring: [0; QUEUE_SIZE as usize] },
    _pad: [0; 4096 - core::mem::size_of::<[VringDesc; QUEUE_SIZE as usize]>() - core::mem::size_of::<VringAvail>()],
    used: VringUsed { flags: 0, idx: 0, ring: [VringUsedElem { id: 0, len: 0 }; QUEUE_SIZE as usize] },
};

static mut PCI_TX_BUFFER: TxBuffer = TxBuffer { data: [0; 256] };
static mut PCI_TX_IDX: u16 = 0;
static mut PCI_LAST_USED_IDX: u16 = 0;

/// Virtio PCI capability structure
#[derive(Debug, Clone, Copy)]
struct VirtioPciCap {
    cap_type: u8,
    bar: u8,
    offset: u32,
    length: u32,
    notify_off_multiplier: u32, // Only for notify cap
}

/// Virtio PCI console driver
pub struct VirtioPciConsole {
    // BAR0 base address (mapped MMIO)
    bar0: u64,
    // Common config offset within BAR
    common_cfg_offset: u32,
    // Notify config offset within BAR
    notify_cfg_offset: u32,
    // Notify offset multiplier
    notify_off_multiplier: u32,
    // Queue notify offset (from common cfg)
    queue_notify_off: u16,
}

impl VirtioPciConsole {
    /// Try to initialize virtio-pci console at given ECAM base
    pub fn try_new(ecam_base: u64, bus: u8, device: u8, function: u8) -> Option<Self> {
        unsafe {
            let config_base = ecam_base
                + ((bus as u64) << 20)
                + ((device as u64) << 15)
                + ((function as u64) << 12);

            // Check vendor/device
            let vendor_id = read_volatile(config_base as *const u16);
            if vendor_id != VIRTIO_VENDOR_ID {
                return None;
            }

            let device_id = read_volatile((config_base + PCI_DEVICE_ID as u64) as *const u16);
            // Only accept console device: 0x1003 (transitional) or 0x1043 (modern)
            if device_id != 0x1003 && device_id != 0x1043 {
                return None;
            }

            // Enable memory access and bus mastering
            let cmd = read_volatile((config_base + PCI_COMMAND as u64) as *const u16);
            write_volatile(
                (config_base + PCI_COMMAND as u64) as *mut u16,
                cmd | PCI_COMMAND_MEMORY | PCI_COMMAND_BUS_MASTER,
            );

            // Read BAR0
            let bar0_low = read_volatile((config_base + PCI_BAR0 as u64) as *const u32);
            let bar0 = if (bar0_low & 0x6) == 0x4 {
                // 64-bit BAR
                let bar0_high = read_volatile((config_base + PCI_BAR0 as u64 + 4) as *const u32);
                ((bar0_high as u64) << 32) | ((bar0_low & !0xf) as u64)
            } else {
                (bar0_low & !0xf) as u64
            };

            if bar0 == 0 {
                return None;
            }

            // Parse capabilities
            let status = read_volatile((config_base + PCI_STATUS as u64) as *const u16);
            if (status & 0x10) == 0 {
                // No capabilities list
                return None;
            }

            let mut cap_ptr = read_volatile((config_base + PCI_CAP_PTR as u64) as *const u8);
            let mut common_cap: Option<VirtioPciCap> = None;
            let mut notify_cap: Option<VirtioPciCap> = None;

            // Walk capability list
            while cap_ptr != 0 {
                let cap_id = read_volatile((config_base + cap_ptr as u64) as *const u8);
                let cap_next = read_volatile((config_base + cap_ptr as u64 + 1) as *const u8);

                // Vendor-specific capability (0x09) is virtio
                if cap_id == 0x09 {
                    let cfg_type = read_volatile((config_base + cap_ptr as u64 + 3) as *const u8);
                    let bar = read_volatile((config_base + cap_ptr as u64 + 4) as *const u8);
                    let offset = read_volatile((config_base + cap_ptr as u64 + 8) as *const u32);
                    let length = read_volatile((config_base + cap_ptr as u64 + 12) as *const u32);

                    let cap = VirtioPciCap {
                        cap_type: cfg_type,
                        bar,
                        offset,
                        length,
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

            // Both must be in BAR0
            if common.bar != 0 || notify.bar != 0 {
                return None;
            }

            let common_base = bar0 + common.offset as u64;
            let notify_base = bar0 + notify.offset as u64;

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

            // 4. Read features (we don't need any)
            write_volatile((common_base + VIRTIO_PCI_COMMON_DFSELECT as u64) as *mut u32, 0);
            let _features = read_volatile((common_base + VIRTIO_PCI_COMMON_DF as u64) as *const u32);

            // 5. Write features (accept none)
            write_volatile((common_base + VIRTIO_PCI_COMMON_GFSELECT as u64) as *mut u32, 0);
            write_volatile((common_base + VIRTIO_PCI_COMMON_GF as u64) as *mut u32, 0);

            // 6. Features OK
            write_volatile(
                (common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
            );
            fence(Ordering::SeqCst);

            // Verify features OK
            let status = read_volatile((common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *const u8);
            if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
                return None;
            }

            // 7. Setup transmit queue (queue 1 for console)
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SELECT as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            let queue_size_max = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *const u16);
            if queue_size_max == 0 {
                return None;
            }

            // Set queue size
            let actual_size = if queue_size_max < QUEUE_SIZE { queue_size_max } else { QUEUE_SIZE };
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *mut u16, actual_size);

            // Set queue addresses
            let desc_addr = &raw const PCI_TX_QUEUE.descs as u64;
            let avail_addr = &raw const PCI_TX_QUEUE.avail as u64;
            let used_addr = &raw const PCI_TX_QUEUE.used as u64;

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCLO as u64) as *mut u32, desc_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCHI as u64) as *mut u32, (desc_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILLO as u64) as *mut u32, avail_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILHI as u64) as *mut u32, (avail_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDLO as u64) as *mut u32, used_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDHI as u64) as *mut u32, (used_addr >> 32) as u32);

            // Get queue notify offset
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

            Some(VirtioPciConsole {
                bar0,
                common_cfg_offset: common.offset,
                notify_cfg_offset: notify.offset,
                notify_off_multiplier: notify.notify_off_multiplier,
                queue_notify_off,
            })
        }
    }

    /// Write a single character
    pub fn putc(&self, c: u8) {
        self.write(&[c]);
    }

    /// Write a string
    pub fn puts(&self, s: &str) {
        self.write(s.as_bytes());
    }

    /// Write bytes
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        unsafe {
            fence(Ordering::SeqCst);

            // Copy data to buffer (truncate if too large)
            let len = data.len().min(PCI_TX_BUFFER.data.len());
            for i in 0..len {
                PCI_TX_BUFFER.data[i] = data[i];
            }

            let idx = PCI_TX_IDX % QUEUE_SIZE;

            // Set up descriptor
            PCI_TX_QUEUE.descs[idx as usize] = VringDesc {
                addr: PCI_TX_BUFFER.data.as_ptr() as u64,
                len: len as u32,
                flags: 0,
                next: 0,
            };

            // Add to available ring
            let avail_idx = PCI_TX_QUEUE.avail.idx;
            PCI_TX_QUEUE.avail.ring[(avail_idx % QUEUE_SIZE) as usize] = idx;
            fence(Ordering::SeqCst);
            PCI_TX_QUEUE.avail.idx = avail_idx.wrapping_add(1);
            fence(Ordering::SeqCst);

            // Notify device
            let notify_addr = self.bar0
                + self.notify_cfg_offset as u64
                + (self.queue_notify_off as u64 * self.notify_off_multiplier as u64);
            write_volatile(notify_addr as *mut u16, 1); // queue index 1
            fence(Ordering::SeqCst);

            PCI_TX_IDX = PCI_TX_IDX.wrapping_add(1);

            // Brief wait for completion
            for _ in 0..10000 {
                fence(Ordering::SeqCst);
                if PCI_TX_QUEUE.used.idx != PCI_LAST_USED_IDX {
                    PCI_LAST_USED_IDX = PCI_TX_QUEUE.used.idx;
                    break;
                }
                core::hint::spin_loop();
            }
        }
    }
}

/// Scan for virtio-pci console device
pub fn find_virtio_pci_console(ecam_base: u64) -> Option<VirtioPciConsole> {
    // Scan buses
    for bus in 0..4u8 {
        for device in 0..32u8 {
            if let Some(console) = VirtioPciConsole::try_new(ecam_base, bus, device, 0) {
                return Some(console);
            }
        }
    }
    None
}

/// Common ECAM addresses to try
pub const ECAM_ADDRESSES: &[u64] = &[
    // Standard locations
    0x3f000000,     // QEMU virt default
    0x40000000,     // Alternative
    0x30000000,     // Lower
    0x10000000,     // PCI MMIO region start
    // Apple Silicon / VZ specific guesses
    0x00000000,     // Base of address space
    0x01000000,     // Low memory
    0x02000000,
    0x08000000,
    0x09000000,     // Near UART
    0x0a000000,     // Virtio MMIO area
    0x0b000000,
    0x0c000000,
    0x0d000000,
    0x0e000000,
    0x0f000000,
    // High memory
    0x20000000,
    0x50000000,
    0x60000000,
    0x70000000,
    0x80000000,
    0x90000000,
    0xa0000000,
    0xb0000000,
    0xc0000000,
    0xd0000000,
    0xe0000000,
    0xf0000000,
];

/// Probe all known ECAM addresses for virtio console
pub fn probe_virtio_pci() -> Option<VirtioPciConsole> {
    for &ecam in ECAM_ADDRESSES {
        // Quick check if ECAM looks valid
        unsafe {
            let vendor = read_volatile(ecam as *const u16);
            if vendor == 0 || vendor == 0xffff {
                continue;
            }
        }
        if let Some(console) = find_virtio_pci_console(ecam) {
            return Some(console);
        }
    }
    None
}
