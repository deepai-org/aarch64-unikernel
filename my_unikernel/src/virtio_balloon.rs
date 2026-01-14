//! VirtIO Balloon device driver
//!
//! Memory ballooning allows the host to reclaim memory from the guest.
//! Device IDs: 0x1005 (transitional), 0x1045 (modern)

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

const VIRTIO_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_BALLOON_TRANSITIONAL: u16 = 0x1005;
const VIRTIO_BALLOON_MODERN: u16 = 0x1045;

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

const QUEUE_SIZE: u16 = 8;
const PAGE_SIZE: u64 = 4096;

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

// Inflate queue (give pages to host)
static mut INFLATE_QUEUE_DESCS: [VringDesc; QUEUE_SIZE as usize] =
    [VringDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE as usize];
static mut INFLATE_QUEUE_AVAIL: VringAvail = VringAvail {
    flags: 0, idx: 0, ring: [0; QUEUE_SIZE as usize]
};
static mut INFLATE_QUEUE_USED: VringUsed = VringUsed {
    flags: 0, idx: 0, ring: [VringUsedElem { id: 0, len: 0 }; QUEUE_SIZE as usize]
};

// Deflate queue (get pages back from host)
static mut DEFLATE_QUEUE_DESCS: [VringDesc; QUEUE_SIZE as usize] =
    [VringDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE as usize];
static mut DEFLATE_QUEUE_AVAIL: VringAvail = VringAvail {
    flags: 0, idx: 0, ring: [0; QUEUE_SIZE as usize]
};
static mut DEFLATE_QUEUE_USED: VringUsed = VringUsed {
    flags: 0, idx: 0, ring: [VringUsedElem { id: 0, len: 0 }; QUEUE_SIZE as usize]
};

// Page frame numbers buffer (balloon uses PFNs, not addresses)
static mut PFN_BUFFER: [u32; 16] = [0; 16];

static mut INFLATE_IDX: u16 = 0;
static mut DEFLATE_IDX: u16 = 0;
static mut INFLATE_LAST_USED: u16 = 0;
static mut DEFLATE_LAST_USED: u16 = 0;

#[derive(Clone, Copy)]
struct VirtioCap {
    bar: u8,
    offset: u32,
    notify_off_multiplier: u32,
}

/// VirtIO Balloon driver
pub struct VirtioBalloon {
    bar0: u64,
    notify_offset: u32,
    notify_multiplier: u32,
    inflate_notify_off: u16,
    deflate_notify_off: u16,
    device_cfg_offset: Option<u32>,
    num_pages: u32,      // Current balloon size
    actual_pages: u32,   // Actual inflated pages
}

impl VirtioBalloon {
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

        // Setup inflate queue (0)
        write_volatile((common_base + 22) as *mut u16, 0);
        fence(Ordering::SeqCst);

        let queue_size_max = read_volatile((common_base + 24) as *const u16);
        if queue_size_max == 0 { return None; }

        let actual_size = queue_size_max.min(QUEUE_SIZE);
        write_volatile((common_base + 24) as *mut u16, actual_size);

        let desc_addr = &raw const INFLATE_QUEUE_DESCS as u64;
        let avail_addr = &raw const INFLATE_QUEUE_AVAIL as u64;
        let used_addr = &raw const INFLATE_QUEUE_USED as u64;

        write_volatile((common_base + 32) as *mut u32, desc_addr as u32);
        write_volatile((common_base + 36) as *mut u32, (desc_addr >> 32) as u32);
        write_volatile((common_base + 40) as *mut u32, avail_addr as u32);
        write_volatile((common_base + 44) as *mut u32, (avail_addr >> 32) as u32);
        write_volatile((common_base + 48) as *mut u32, used_addr as u32);
        write_volatile((common_base + 52) as *mut u32, (used_addr >> 32) as u32);

        let inflate_notify_off = read_volatile((common_base + 30) as *const u16);
        write_volatile((common_base + 28) as *mut u16, 1);
        fence(Ordering::SeqCst);

        // Setup deflate queue (1)
        write_volatile((common_base + 22) as *mut u16, 1);
        fence(Ordering::SeqCst);

        let queue_size_max = read_volatile((common_base + 24) as *const u16);
        if queue_size_max == 0 { return None; }

        let actual_size = queue_size_max.min(QUEUE_SIZE);
        write_volatile((common_base + 24) as *mut u16, actual_size);

        let desc_addr = &raw const DEFLATE_QUEUE_DESCS as u64;
        let avail_addr = &raw const DEFLATE_QUEUE_AVAIL as u64;
        let used_addr = &raw const DEFLATE_QUEUE_USED as u64;

        write_volatile((common_base + 32) as *mut u32, desc_addr as u32);
        write_volatile((common_base + 36) as *mut u32, (desc_addr >> 32) as u32);
        write_volatile((common_base + 40) as *mut u32, avail_addr as u32);
        write_volatile((common_base + 44) as *mut u32, (avail_addr >> 32) as u32);
        write_volatile((common_base + 48) as *mut u32, used_addr as u32);
        write_volatile((common_base + 52) as *mut u32, (used_addr >> 32) as u32);

        let deflate_notify_off = read_volatile((common_base + 30) as *const u16);
        write_volatile((common_base + 28) as *mut u16, 1);
        fence(Ordering::SeqCst);

        // Driver OK
        write_volatile((common_base + 20) as *mut u8, 0x0F);
        fence(Ordering::SeqCst);

        Some(VirtioBalloon {
            bar0: notify_base,
            notify_offset: 0,
            notify_multiplier: modern.notify_mult,
            inflate_notify_off,
            deflate_notify_off,
            device_cfg_offset: None,
            num_pages: 0,
            actual_pages: 0,
        })
    }

    /// Find and initialize balloon device on PCI bus (legacy)
    pub fn find(ecam: u64) -> Option<Self> {
        for dev in 0u8..32 {
            let config_base = ecam + ((dev as u64) << 15);

            unsafe {
                let vendor = read_volatile(config_base as *const u16);
                if vendor != VIRTIO_VENDOR_ID {
                    continue;
                }

                let device_id = read_volatile((config_base + 2) as *const u16);
                if device_id != VIRTIO_BALLOON_TRANSITIONAL && device_id != VIRTIO_BALLOON_MODERN {
                    continue;
                }

                if let Some(balloon) = Self::try_new(config_base, dev) {
                    return Some(balloon);
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

            // 7. Setup inflate queue (queue 0)
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SELECT as u64) as *mut u16, 0);
            fence(Ordering::SeqCst);

            let queue_size_max = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *const u16);
            if queue_size_max == 0 {
                return None;
            }

            let actual_size = queue_size_max.min(QUEUE_SIZE);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *mut u16, actual_size);

            let desc_addr = &raw const INFLATE_QUEUE_DESCS as u64;
            let avail_addr = &raw const INFLATE_QUEUE_AVAIL as u64;
            let used_addr = &raw const INFLATE_QUEUE_USED as u64;

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCLO as u64) as *mut u32, desc_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCHI as u64) as *mut u32, (desc_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILLO as u64) as *mut u32, avail_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILHI as u64) as *mut u32, (avail_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDLO as u64) as *mut u32, used_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDHI as u64) as *mut u32, (used_addr >> 32) as u32);

            let inflate_notify_off = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_NOFF as u64) as *const u16);

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_ENABLE as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            // 8. Setup deflate queue (queue 1)
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SELECT as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            let queue_size_max = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *const u16);
            if queue_size_max == 0 {
                return None;
            }

            let actual_size = queue_size_max.min(QUEUE_SIZE);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *mut u16, actual_size);

            let desc_addr = &raw const DEFLATE_QUEUE_DESCS as u64;
            let avail_addr = &raw const DEFLATE_QUEUE_AVAIL as u64;
            let used_addr = &raw const DEFLATE_QUEUE_USED as u64;

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCLO as u64) as *mut u32, desc_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCHI as u64) as *mut u32, (desc_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILLO as u64) as *mut u32, avail_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILHI as u64) as *mut u32, (avail_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDLO as u64) as *mut u32, used_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDHI as u64) as *mut u32, (used_addr >> 32) as u32);

            let deflate_notify_off = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_NOFF as u64) as *const u16);

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_ENABLE as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            // 9. Driver OK
            write_volatile(
                (common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK,
            );
            fence(Ordering::SeqCst);

            // Read initial balloon config if available
            let (num_pages, device_cfg_offset) = if let Some(dev_cfg) = device_cap {
                if dev_cfg.bar == 0 {
                    let dev_base = bar0 + dev_cfg.offset as u64;
                    // Balloon config: num_pages (u32), actual (u32)
                    let num = read_volatile(dev_base as *const u32);
                    (num, Some(dev_cfg.offset))
                } else {
                    (0, None)
                }
            } else {
                (0, None)
            };

            Some(VirtioBalloon {
                bar0,
                notify_offset: notify.offset,
                notify_multiplier: notify.notify_off_multiplier,
                inflate_notify_off,
                deflate_notify_off,
                device_cfg_offset,
                num_pages,
                actual_pages: 0,
            })
        }
    }

    /// Get requested balloon size in pages
    pub fn num_pages(&self) -> u32 {
        self.num_pages
    }

    /// Get actual inflated pages
    pub fn actual(&self) -> u32 {
        self.actual_pages
    }

    /// Read current config from device
    pub fn update_config(&mut self) {
        if let Some(offset) = self.device_cfg_offset {
            unsafe {
                let dev_base = self.bar0 + offset as u64;
                self.num_pages = read_volatile(dev_base as *const u32);
            }
        }
    }

    /// Inflate balloon by giving pages to host
    /// page_addr: physical address of a page to give up
    pub fn inflate(&mut self, page_addrs: &[u64]) -> bool {
        if page_addrs.is_empty() || page_addrs.len() > 16 {
            return false;
        }

        unsafe {
            // Convert addresses to PFNs (page frame numbers)
            for (i, &addr) in page_addrs.iter().enumerate() {
                PFN_BUFFER[i] = (addr / PAGE_SIZE) as u32;
            }

            let idx = INFLATE_IDX % QUEUE_SIZE;

            INFLATE_QUEUE_DESCS[idx as usize] = VringDesc {
                addr: PFN_BUFFER.as_ptr() as u64,
                len: (page_addrs.len() * 4) as u32,
                flags: 0,
                next: 0,
            };

            fence(Ordering::SeqCst);

            let avail_idx = read_volatile(&INFLATE_QUEUE_AVAIL.idx);
            write_volatile(
                &raw mut INFLATE_QUEUE_AVAIL.ring[(avail_idx % QUEUE_SIZE) as usize],
                idx,
            );
            fence(Ordering::SeqCst);
            write_volatile(&raw mut INFLATE_QUEUE_AVAIL.idx, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Notify
            let notify_addr = self.bar0
                + self.notify_offset as u64
                + (self.inflate_notify_off as u64 * self.notify_multiplier as u64);
            write_volatile(notify_addr as *mut u16, 0);
            fence(Ordering::SeqCst);

            INFLATE_IDX = INFLATE_IDX.wrapping_add(1);

            // Wait for completion
            for _ in 0..100_000 {
                fence(Ordering::SeqCst);
                let used_idx = read_volatile(&INFLATE_QUEUE_USED.idx);
                if used_idx != INFLATE_LAST_USED {
                    INFLATE_LAST_USED = used_idx;
                    self.actual_pages += page_addrs.len() as u32;
                    return true;
                }
                core::hint::spin_loop();
            }

            false
        }
    }

    /// Deflate balloon by getting pages back from host
    pub fn deflate(&mut self, page_addrs: &[u64]) -> bool {
        if page_addrs.is_empty() || page_addrs.len() > 16 {
            return false;
        }

        unsafe {
            // Convert addresses to PFNs
            for (i, &addr) in page_addrs.iter().enumerate() {
                PFN_BUFFER[i] = (addr / PAGE_SIZE) as u32;
            }

            let idx = DEFLATE_IDX % QUEUE_SIZE;

            DEFLATE_QUEUE_DESCS[idx as usize] = VringDesc {
                addr: PFN_BUFFER.as_ptr() as u64,
                len: (page_addrs.len() * 4) as u32,
                flags: 0,
                next: 0,
            };

            fence(Ordering::SeqCst);

            let avail_idx = read_volatile(&DEFLATE_QUEUE_AVAIL.idx);
            write_volatile(
                &raw mut DEFLATE_QUEUE_AVAIL.ring[(avail_idx % QUEUE_SIZE) as usize],
                idx,
            );
            fence(Ordering::SeqCst);
            write_volatile(&raw mut DEFLATE_QUEUE_AVAIL.idx, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Notify
            let notify_addr = self.bar0
                + self.notify_offset as u64
                + (self.deflate_notify_off as u64 * self.notify_multiplier as u64);
            write_volatile(notify_addr as *mut u16, 1);
            fence(Ordering::SeqCst);

            DEFLATE_IDX = DEFLATE_IDX.wrapping_add(1);

            // Wait for completion
            for _ in 0..100_000 {
                fence(Ordering::SeqCst);
                let used_idx = read_volatile(&DEFLATE_QUEUE_USED.idx);
                if used_idx != DEFLATE_LAST_USED {
                    DEFLATE_LAST_USED = used_idx;
                    self.actual_pages = self.actual_pages.saturating_sub(page_addrs.len() as u32);
                    return true;
                }
                core::hint::spin_loop();
            }

            false
        }
    }

    /// Test balloon by inflating and deflating
    pub fn test_balloon(&mut self) -> BalloonTestResult {
        // Use a fixed "dummy" page address for testing
        // In a real system, we'd allocate real pages
        let test_page: u64 = 0x8100_0000; // High memory, unlikely to conflict

        // Try to inflate (give page to host)
        let inflate_ok = self.inflate(&[test_page]);

        // Try to deflate (get it back)
        let deflate_ok = self.deflate(&[test_page]);

        // Read config
        self.update_config();

        BalloonTestResult {
            inflate_ok,
            deflate_ok,
            num_pages: self.num_pages,
            actual_pages: self.actual_pages,
            init_ok: true,
        }
    }
}

/// Balloon test result
pub struct BalloonTestResult {
    pub inflate_ok: bool,
    pub deflate_ok: bool,
    pub num_pages: u32,
    pub actual_pages: u32,
    pub init_ok: bool,
}
