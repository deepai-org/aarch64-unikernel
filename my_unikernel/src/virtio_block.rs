//! VirtIO Block device driver
//!
//! Provides sector-based read/write access to virtual disk.
//! Device IDs: 0x1001 (transitional), 0x1042 (modern)

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

const VIRTIO_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_BLK_TRANSITIONAL: u16 = 0x1001;
const VIRTIO_BLK_MODERN: u16 = 0x1042;

// PCI config space offsets
const PCI_COMMAND: usize = 0x04;
const PCI_STATUS: usize = 0x06;
const PCI_BAR0: usize = 0x10;
const PCI_CAP_PTR: usize = 0x34;

const PCI_COMMAND_MEMORY: u16 = 0x02;
const PCI_COMMAND_BUS_MASTER: u16 = 0x04;

// VirtIO PCI capability types
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

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

// Block request types
const VIRTIO_BLK_T_IN: u32 = 0;   // Read
const VIRTIO_BLK_T_OUT: u32 = 1;  // Write

// Block request status
const VIRTIO_BLK_S_OK: u8 = 0;

// Descriptor flags
const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

const QUEUE_SIZE: u16 = 8;
const SECTOR_SIZE: usize = 512;

/// Block device request header
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VirtioBlkReqHeader {
    req_type: u32,
    reserved: u32,
    sector: u64,
}

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

// Static buffers
static mut BLK_QUEUE_DESCS: [VringDesc; QUEUE_SIZE as usize] =
    [VringDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE as usize];
static mut BLK_QUEUE_AVAIL: VringAvail = VringAvail {
    flags: 0, idx: 0, ring: [0; QUEUE_SIZE as usize]
};
static mut BLK_QUEUE_USED: VringUsed = VringUsed {
    flags: 0, idx: 0, ring: [VringUsedElem { id: 0, len: 0 }; QUEUE_SIZE as usize]
};

// Request buffers
static mut BLK_REQ_HEADER: VirtioBlkReqHeader = VirtioBlkReqHeader {
    req_type: 0, reserved: 0, sector: 0
};
static mut BLK_DATA_BUFFER: [u8; SECTOR_SIZE] = [0; SECTOR_SIZE];
static mut BLK_STATUS: u8 = 0xFF;
static mut BLK_DESC_IDX: u16 = 0;
static mut BLK_LAST_USED: u16 = 0;

#[derive(Clone, Copy)]
struct VirtioCap {
    bar: u8,
    offset: u32,
    notify_off_multiplier: u32,
}

/// VirtIO Block driver
pub struct VirtioBlock {
    bar0: u64,
    notify_offset: u32,
    notify_multiplier: u32,
    queue_notify_off: u16,
    capacity: u64,  // in sectors
}

impl VirtioBlock {
    /// Create from pre-configured VirtioModern transport
    pub unsafe fn from_modern(modern: &crate::pci::VirtioModern, _ecam_addr: u64) -> Option<Self> {
        use core::sync::atomic::{fence, Ordering};

        let common_base = modern.common;
        let notify_base = modern.notify;
        let device_base = modern.device;

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

        // Read capacity from device config
        let capacity = if device_base != 0 {
            read_volatile(device_base as *const u64)
        } else {
            0
        };

        // Setup queue 0
        write_volatile((common_base + 22) as *mut u16, 0);
        fence(Ordering::SeqCst);

        let queue_size_max = read_volatile((common_base + 24) as *const u16);
        if queue_size_max == 0 { return None; }

        let actual_size = queue_size_max.min(QUEUE_SIZE);
        write_volatile((common_base + 24) as *mut u16, actual_size);

        let desc_addr = &raw const BLK_QUEUE_DESCS as u64;
        let avail_addr = &raw const BLK_QUEUE_AVAIL as u64;
        let used_addr = &raw const BLK_QUEUE_USED as u64;

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

        Some(VirtioBlock {
            bar0: notify_base,
            notify_offset: 0,
            notify_multiplier: modern.notify_mult,
            queue_notify_off,
            capacity,
        })
    }

    /// Find and initialize block device on PCI bus (legacy)
    pub fn find(ecam: u64) -> Option<Self> {
        for dev in 0u8..32 {
            let config_base = ecam + ((dev as u64) << 15);

            unsafe {
                let vendor = read_volatile(config_base as *const u16);
                if vendor != VIRTIO_VENDOR_ID {
                    continue;
                }

                let device_id = read_volatile((config_base + 2) as *const u16);
                if device_id != VIRTIO_BLK_TRANSITIONAL && device_id != VIRTIO_BLK_MODERN {
                    continue;
                }

                if let Some(blk) = Self::try_new(config_base, dev) {
                    return Some(blk);
                }
            }
        }
        None
    }

    fn try_new(config_base: u64, _slot: u8) -> Option<Self> {
        unsafe {
            // Enable memory access and bus mastering
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
                return None;
            }

            let mut cap_ptr = read_volatile((config_base + PCI_CAP_PTR as u64) as *const u8);
            let mut common_cap: Option<VirtioCap> = None;
            let mut notify_cap: Option<VirtioCap> = None;
            let mut device_cap: Option<VirtioCap> = None;

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
                        VIRTIO_PCI_CAP_DEVICE_CFG => device_cap = Some(cap),
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

            // Read device capacity if device config is available
            let capacity = if let Some(dev_cfg) = device_cap {
                if dev_cfg.bar == 0 {
                    let dev_base = bar0 + dev_cfg.offset as u64;
                    read_volatile(dev_base as *const u64)
                } else {
                    0
                }
            } else {
                0
            };

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

            // 4. Read features
            write_volatile((common_base + VIRTIO_PCI_COMMON_DFSELECT as u64) as *mut u32, 0);
            let _features = read_volatile((common_base + VIRTIO_PCI_COMMON_DF as u64) as *const u32);

            // 5. Accept no optional features, but MUST set VERSION_1
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

            let status = read_volatile((common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *const u8);
            if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
                return None;
            }

            // 7. Setup queue 0
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SELECT as u64) as *mut u16, 0);
            fence(Ordering::SeqCst);

            let queue_size_max = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *const u16);
            if queue_size_max == 0 {
                return None;
            }

            let actual_size = queue_size_max.min(QUEUE_SIZE);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *mut u16, actual_size);

            let desc_addr = &raw const BLK_QUEUE_DESCS as u64;
            let avail_addr = &raw const BLK_QUEUE_AVAIL as u64;
            let used_addr = &raw const BLK_QUEUE_USED as u64;

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCLO as u64) as *mut u32, desc_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCHI as u64) as *mut u32, (desc_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILLO as u64) as *mut u32, avail_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILHI as u64) as *mut u32, (avail_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDLO as u64) as *mut u32, used_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDHI as u64) as *mut u32, (used_addr >> 32) as u32);

            let queue_notify_off = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_NOFF as u64) as *const u16);

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_ENABLE as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            // 8. Driver OK
            write_volatile(
                (common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK,
            );
            fence(Ordering::SeqCst);

            Some(VirtioBlock {
                bar0,
                notify_offset: notify.offset,
                notify_multiplier: notify.notify_off_multiplier,
                queue_notify_off,
                capacity,
            })
        }
    }

    /// Get disk capacity in sectors
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Read a sector from disk
    pub fn read_sector(&self, sector: u64, buf: &mut [u8; SECTOR_SIZE]) -> bool {
        self.do_request(VIRTIO_BLK_T_IN, sector, buf)
    }

    /// Write a sector to disk
    pub fn write_sector(&self, sector: u64, buf: &[u8; SECTOR_SIZE]) -> bool {
        // Copy to static buffer for write
        unsafe {
            for i in 0..SECTOR_SIZE {
                BLK_DATA_BUFFER[i] = buf[i];
            }
        }
        let mut dummy = [0u8; SECTOR_SIZE];
        self.do_request(VIRTIO_BLK_T_OUT, sector, &mut dummy)
    }

    fn do_request(&self, req_type: u32, sector: u64, buf: &mut [u8; SECTOR_SIZE]) -> bool {
        unsafe {
            // Setup request header
            BLK_REQ_HEADER.req_type = req_type;
            BLK_REQ_HEADER.reserved = 0;
            BLK_REQ_HEADER.sector = sector;
            BLK_STATUS = 0xFF;

            fence(Ordering::SeqCst);

            // We need 3 descriptors chained:
            // 1. Header (device-readable)
            // 2. Data buffer (device-readable for write, device-writable for read)
            // 3. Status byte (device-writable)

            let base_idx = (BLK_DESC_IDX % QUEUE_SIZE) as usize;
            let idx0 = base_idx;
            let idx1 = (base_idx + 1) % QUEUE_SIZE as usize;
            let idx2 = (base_idx + 2) % QUEUE_SIZE as usize;

            // Descriptor 0: Header
            BLK_QUEUE_DESCS[idx0] = VringDesc {
                addr: &raw const BLK_REQ_HEADER as u64,
                len: core::mem::size_of::<VirtioBlkReqHeader>() as u32,
                flags: VRING_DESC_F_NEXT,
                next: idx1 as u16,
            };

            // Descriptor 1: Data buffer
            if req_type == VIRTIO_BLK_T_IN {
                // Read: device writes to buffer
                BLK_QUEUE_DESCS[idx1] = VringDesc {
                    addr: BLK_DATA_BUFFER.as_ptr() as u64,
                    len: SECTOR_SIZE as u32,
                    flags: VRING_DESC_F_WRITE | VRING_DESC_F_NEXT,
                    next: idx2 as u16,
                };
            } else {
                // Write: device reads from buffer
                BLK_QUEUE_DESCS[idx1] = VringDesc {
                    addr: BLK_DATA_BUFFER.as_ptr() as u64,
                    len: SECTOR_SIZE as u32,
                    flags: VRING_DESC_F_NEXT,
                    next: idx2 as u16,
                };
            }

            // Descriptor 2: Status byte
            BLK_QUEUE_DESCS[idx2] = VringDesc {
                addr: &raw mut BLK_STATUS as u64,
                len: 1,
                flags: VRING_DESC_F_WRITE,
                next: 0,
            };

            fence(Ordering::SeqCst);

            // Add first descriptor to available ring
            let avail_idx = read_volatile(&BLK_QUEUE_AVAIL.idx);
            write_volatile(
                &raw mut BLK_QUEUE_AVAIL.ring[(avail_idx % QUEUE_SIZE) as usize],
                idx0 as u16,
            );
            fence(Ordering::SeqCst);
            write_volatile(&raw mut BLK_QUEUE_AVAIL.idx, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Notify device
            let notify_addr = self.bar0
                + self.notify_offset as u64
                + (self.queue_notify_off as u64 * self.notify_multiplier as u64);
            write_volatile(notify_addr as *mut u16, 0);
            fence(Ordering::SeqCst);

            BLK_DESC_IDX = BLK_DESC_IDX.wrapping_add(3);

            // Wait for completion
            for _ in 0..1_000_000 {
                fence(Ordering::SeqCst);
                let used_idx = read_volatile(&BLK_QUEUE_USED.idx);
                if used_idx != BLK_LAST_USED {
                    BLK_LAST_USED = used_idx;
                    break;
                }
                core::hint::spin_loop();
            }

            fence(Ordering::SeqCst);

            // Check status
            let status = read_volatile(&BLK_STATUS);
            if status != VIRTIO_BLK_S_OK {
                return false;
            }

            // Copy data for read
            if req_type == VIRTIO_BLK_T_IN {
                for i in 0..SECTOR_SIZE {
                    buf[i] = read_volatile(&BLK_DATA_BUFFER[i]);
                }
            }

            true
        }
    }

    /// Test block device by writing and reading back a pattern
    pub fn test_read_write(&self) -> BlockTestResult {
        // Create test pattern
        let mut write_buf = [0u8; SECTOR_SIZE];
        for i in 0..SECTOR_SIZE {
            write_buf[i] = ((i * 7 + 13) & 0xFF) as u8;
        }

        // Write to sector 0
        let write_ok = self.write_sector(0, &write_buf);

        // Read it back
        let mut read_buf = [0u8; SECTOR_SIZE];
        let read_ok = self.read_sector(0, &mut read_buf);

        // Compare
        let mut matches = 0usize;
        for i in 0..SECTOR_SIZE {
            if read_buf[i] == write_buf[i] {
                matches += 1;
            }
        }

        BlockTestResult {
            capacity: self.capacity,
            write_ok,
            read_ok,
            data_matches: matches,
            test_passed: write_ok && read_ok && matches == SECTOR_SIZE,
        }
    }
}

/// Block device test result
pub struct BlockTestResult {
    pub capacity: u64,
    pub write_ok: bool,
    pub read_ok: bool,
    pub data_matches: usize,
    pub test_passed: bool,
}
