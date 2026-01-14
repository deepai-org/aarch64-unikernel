//! Virtio Console (virtio-console over virtio-pci) driver for Apple VZ
//!
//! Provides a simple polled TX path for debug prints (and optional RX polling).
//! Assumes virtio-pci (not virtio-mmio).
//!
//! Notes:
//! - virtio device id for "console" is 3, so modern PCI device id is 0x1040 + 3 = 0x1043.
//! - Only negotiates VIRTIO_F_VERSION_1. Split ring only.
//! - Uses fixed DMA-visible RAM for vrings + buffers (similar to your GPU driver).
//! - Includes a tiny PCI BAR allocator for VZ where BARs start at 0.
//!
//! Integrate by calling `find_virtio_console()` early, storing it globally, and
//! routing your `putc/puts` to `console.write_*`.
//!
//! If you want input: call `console.poll_read(...)`.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// -------------------------- PCI constants --------------------------

const PCI_COMMAND: usize = 0x04;
const PCI_STATUS: usize = 0x06;
const PCI_BAR0: usize = 0x10;
const PCI_CAP_PTR: usize = 0x34;

const PCI_STATUS_CAP_LIST: u16 = 0x10;

// Virtio vendor/device IDs
const VIRTIO_VENDOR_ID: u16 = 0x1af4;

// virtio device id 3 => 0x1043 for modern virtio-pci
const VIRTIO_CONSOLE_PCI_DEVICE_ID: u16 = 0x1043;
// legacy/transitional often shows up as 0x1003 (depending on platform)
const VIRTIO_CONSOLE_PCI_DEVICE_ID_LEGACY: u16 = 0x1003;

// virtio-pci capability types
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;

// -------------------------- Virtio common cfg offsets --------------------------
// These offsets match the virtio-pci "common configuration" structure.

const VIRTIO_PCI_COMMON_GFSELECT: usize = 0x08;
const VIRTIO_PCI_COMMON_GF: usize = 0x0c;

const VIRTIO_PCI_COMMON_NUM_QUEUES: usize = 0x12;

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

// -------------------------- Virtio status bits --------------------------

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;

// -------------------------- Split ring defs --------------------------

const QUEUE_SIZE: u16 = 16;

const VRING_DESC_F_WRITE: u16 = 2;

// -------------------------- Fixed DMA layout --------------------------
// Pick a region different from your GPU driver. Adjust if you already use 0x8001_xxxx.

const DMA_BASE: u64 = 0x8001_0000;

// Queue 0: RX
const RX_DESC: u64 = DMA_BASE + 0x0000;
const RX_AVAIL: u64 = DMA_BASE + 0x1000;
const RX_USED: u64 = DMA_BASE + 0x2000;

// Queue 1: TX
const TX_DESC: u64 = DMA_BASE + 0x3000;
const TX_AVAIL: u64 = DMA_BASE + 0x4000;
const TX_USED: u64 = DMA_BASE + 0x5000;

const TX_BUF: u64 = DMA_BASE + 0x6000;

// RX buffers: QUEUE_SIZE * RX_BUF_SZ bytes
const RX_BUFS: u64 = DMA_BASE + 0x7000;
const RX_BUF_SZ: usize = 512;

// Size of fixed DMA region we clear on init (conservative)
const DMA_CLEAR_LEN: usize = 0x9000;

// -------------------------- ECAM scan addresses --------------------------

const ECAM_ADDRESSES: &[u64] = &[
    0x40000000, // VZ uses this one
    0x3f000000,
    0x30000000,
    0x50000000,
    0x80000000,
    0xb0000000,
    0xc0000000,
    0xe0000000,
    0xf0000000,
    0x10000000,
    0x20000000,
];

// -------------------------- Small helpers --------------------------

#[inline(always)]
unsafe fn mmio_write_u8(addr: u64, v: u8) {
    write_volatile(addr as *mut u8, v);
}
#[inline(always)]
unsafe fn mmio_read_u8(addr: u64) -> u8 {
    read_volatile(addr as *const u8)
}
#[inline(always)]
unsafe fn mmio_write_u16(addr: u64, v: u16) {
    write_volatile(addr as *mut u16, v);
}
#[inline(always)]
unsafe fn mmio_read_u16(addr: u64) -> u16 {
    read_volatile(addr as *const u16)
}
#[inline(always)]
unsafe fn mmio_write_u32(addr: u64, v: u32) {
    write_volatile(addr as *mut u32, v);
}
#[inline(always)]
unsafe fn mmio_read_u32(addr: u64) -> u32 {
    read_volatile(addr as *const u32)
}
#[inline(always)]
unsafe fn mmio_write_u64(addr: u64, v: u64) {
    write_volatile(addr as *mut u64, v);
}

fn align_up(x: u64, a: u64) -> u64 {
    (x + (a - 1)) & !(a - 1)
}

// Minimal PCI BAR allocator (for VZ where BARs come up as 0).
// Allocates memory BARs sequentially from a chosen MMIO window.
static mut PCI_MMIO_NEXT: u64 = 0x5001_0000; // Start after GPU's 0x50008000

unsafe fn pci_program_bars_if_needed(config_base: u64) {
    let mut bar = 0u8;
    while bar < 6 {
        let bar_reg = config_base + PCI_BAR0 as u64 + (bar as u64) * 4;
        let orig = mmio_read_u32(bar_reg);

        // Only program if zero
        if orig != 0 {
            // Skip high dword for 64-bit BAR
            if (orig & 0x1) == 0 && (orig & 0x6) == 0x4 {
                bar += 2;
            } else {
                bar += 1;
            }
            continue;
        }

        // Probe size
        mmio_write_u32(bar_reg, 0xFFFF_FFFF);
        fence(Ordering::SeqCst);
        let mask_lo = mmio_read_u32(bar_reg);

        // Restore temporarily (we'll set real base next)
        mmio_write_u32(bar_reg, 0);
        fence(Ordering::SeqCst);

        // IO BAR? skip.
        if (mask_lo & 0x1) != 0 {
            bar += 1;
            continue;
        }

        let is_64 = (mask_lo & 0x6) == 0x4;

        let size: u64 = if is_64 && bar < 5 {
            let bar_reg_hi = bar_reg + 4;

            mmio_write_u32(bar_reg, 0xFFFF_FFFF);
            mmio_write_u32(bar_reg_hi, 0xFFFF_FFFF);
            fence(Ordering::SeqCst);

            let mask_lo2 = mmio_read_u32(bar_reg);
            let mask_hi2 = mmio_read_u32(bar_reg_hi);

            // Clear and restore 0
            mmio_write_u32(bar_reg, 0);
            mmio_write_u32(bar_reg_hi, 0);
            fence(Ordering::SeqCst);

            let mask64 = ((mask_hi2 as u64) << 32) | ((mask_lo2 as u64) & !0xFu64);
            if mask64 == 0 {
                0
            } else {
                (!mask64).wrapping_add(1)
            }
        } else {
            let mask32 = (mask_lo as u64) & !0xFu64;
            if mask32 == 0 {
                0
            } else {
                (!mask32).wrapping_add(1) & 0xFFFF_FFFF
            }
        };

        if size == 0 || size == u64::MAX {
            // Can't size; skip
            if is_64 {
                bar += 2;
            } else {
                bar += 1;
            }
            continue;
        }

        // Allocate a base aligned to size.
        let base = align_up(PCI_MMIO_NEXT, size.max(0x1000));
        PCI_MMIO_NEXT = base + size;

        // Program BAR
        mmio_write_u32(bar_reg, (base as u32) & !0xF);
        if is_64 && bar < 5 {
            let bar_reg_hi = bar_reg + 4;
            mmio_write_u32(bar_reg_hi, (base >> 32) as u32);
            bar += 2;
        } else {
            bar += 1;
        }
        fence(Ordering::SeqCst);
    }
}

unsafe fn read_bar(config_base: u64, bar_idx: u8) -> u64 {
    if bar_idx > 5 {
        return 0;
    }
    let bar_reg = config_base + PCI_BAR0 as u64 + (bar_idx as u64) * 4;
    let lo = mmio_read_u32(bar_reg);
    if (lo & 0x1) != 0 {
        // IO BAR (ignore)
        return 0;
    }
    let is_64 = (lo & 0x6) == 0x4;
    if is_64 && bar_idx < 5 {
        let hi = mmio_read_u32(bar_reg + 4);
        ((hi as u64) << 32) | ((lo as u64) & !0xFu64)
    } else {
        (lo as u64) & !0xFu64
    }
}

// -------------------------- Virtio Console --------------------------

pub struct VirtioConsole {
    notify_base: u64,
    notify_mult: u32,

    rx_notify_off: u16,
    tx_notify_off: u16,

    rx_last_used: u16,
    tx_last_used: u16,

    qsize: u16,
}

impl VirtioConsole {
    pub fn try_new(ecam_base: u64, bus: u8, device: u8) -> Option<Self> {
        unsafe {
            let config_base = ecam_base + ((bus as u64) << 20) + ((device as u64) << 15);

            // Read vendor/device from first dword
            let header = mmio_read_u32(config_base);
            let vendor_id = (header & 0xFFFF) as u16;
            let device_id = ((header >> 16) & 0xFFFF) as u16;

            if vendor_id != VIRTIO_VENDOR_ID {
                return None;
            }
            if device_id != VIRTIO_CONSOLE_PCI_DEVICE_ID && device_id != VIRTIO_CONSOLE_PCI_DEVICE_ID_LEGACY {
                return None;
            }

            // DEBUG: Found console device!
            // core::arch::asm!("brk #0xD1"); // Uncomment to verify we reach here

            // Enable PCI command bits: I/O + Mem + BusMaster
            let cmd_ptr = (config_base + PCI_COMMAND as u64) as *mut u16;
            let cmd = read_volatile(cmd_ptr);
            write_volatile(cmd_ptr, cmd | 0x07);
            fence(Ordering::SeqCst);

            // Check BAR0 - if zero, we need to program it
            let bar0 = read_bar(config_base, 0);
            if bar0 == 0 {
                // Program BAR0 at 0x5000C000 (after GPU's 0x50008000-0x5000BFFF)
                mmio_write_u32(config_base + PCI_BAR0 as u64, 0x5000C000);
                fence(Ordering::SeqCst);
            }

            // Capability list present?
            let status = mmio_read_u16(config_base + PCI_STATUS as u64);
            if (status & PCI_STATUS_CAP_LIST) == 0 {
                return None;
            }

            // Walk PCI caps, find virtio common + notify
            let mut cap_ptr = mmio_read_u8(config_base + PCI_CAP_PTR as u64);

            let mut common_cfg: Option<u64> = None;
            let mut notify_cfg: Option<u64> = None;
            let mut notify_mult: u32 = 0;

            while cap_ptr != 0 {
                let cap_id = mmio_read_u8(config_base + cap_ptr as u64);
                let cap_next = mmio_read_u8(config_base + cap_ptr as u64 + 1);

                // 0x09 = vendor-specific cap used by virtio-pci
                if cap_id == 0x09 {
                    let cfg_type = mmio_read_u8(config_base + cap_ptr as u64 + 3);
                    let bar_idx = mmio_read_u8(config_base + cap_ptr as u64 + 4);
                    let offset = mmio_read_u32(config_base + cap_ptr as u64 + 8);

                    let bar_addr = read_bar(config_base, bar_idx);
                    if bar_addr != 0 {
                        let base = bar_addr + offset as u64;
                        match cfg_type {
                            VIRTIO_PCI_CAP_COMMON_CFG => {
                                common_cfg = Some(base);
                            }
                            VIRTIO_PCI_CAP_NOTIFY_CFG => {
                                notify_cfg = Some(base);
                                // notify_off_multiplier at +16
                                notify_mult = mmio_read_u32(config_base + cap_ptr as u64 + 16);
                            }
                            _ => {}
                        }
                    }
                }

                cap_ptr = cap_next;
            }

            let common = common_cfg?;
            let notify = notify_cfg?;
            if notify_mult == 0 {
                return None;
            }

            // Sanity: read num_queues
            let numq = mmio_read_u16(common + VIRTIO_PCI_COMMON_NUM_QUEUES as u64);
            if numq < 2 {
                return None;
            }

            // Reset
            mmio_write_u8(common + VIRTIO_PCI_COMMON_STATUS as u64, 0);
            fence(Ordering::SeqCst);

            // ACK + DRIVER
            mmio_write_u8(common + VIRTIO_PCI_COMMON_STATUS as u64, VIRTIO_STATUS_ACKNOWLEDGE);
            fence(Ordering::SeqCst);
            mmio_write_u8(
                common + VIRTIO_PCI_COMMON_STATUS as u64,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
            );
            fence(Ordering::SeqCst);

            // Feature negotiation: only VIRTIO_F_VERSION_1 (bank 1 bit 0)
            mmio_write_u32(common + VIRTIO_PCI_COMMON_GFSELECT as u64, 0);
            fence(Ordering::SeqCst);
            mmio_write_u32(common + VIRTIO_PCI_COMMON_GF as u64, 0);
            fence(Ordering::SeqCst);

            mmio_write_u32(common + VIRTIO_PCI_COMMON_GFSELECT as u64, 1);
            fence(Ordering::SeqCst);
            mmio_write_u32(common + VIRTIO_PCI_COMMON_GF as u64, 1);
            fence(Ordering::SeqCst);

            // FEATURES_OK
            mmio_write_u8(
                common + VIRTIO_PCI_COMMON_STATUS as u64,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
            );
            fence(Ordering::SeqCst);

            let st = mmio_read_u8(common + VIRTIO_PCI_COMMON_STATUS as u64);
            if (st & VIRTIO_STATUS_FEATURES_OK) == 0 {
                return None;
            }

            // Clear DMA region for rings/buffers
            for i in 0..DMA_CLEAR_LEN {
                write_volatile((DMA_BASE + i as u64) as *mut u8, 0);
            }
            fence(Ordering::SeqCst);

            // Setup RX queue (queue 0)
            let (rx_nof, qsize) = setup_queue(common, 0, QUEUE_SIZE, RX_DESC, RX_AVAIL, RX_USED)?;
            // Setup TX queue (queue 1)
            let (tx_nof, _qsize2) = setup_queue(common, 1, qsize, TX_DESC, TX_AVAIL, TX_USED)?;

            // DRIVER_OK
            mmio_write_u8(
                common + VIRTIO_PCI_COMMON_STATUS as u64,
                VIRTIO_STATUS_ACKNOWLEDGE
                    | VIRTIO_STATUS_DRIVER
                    | VIRTIO_STATUS_FEATURES_OK
                    | VIRTIO_STATUS_DRIVER_OK,
            );
            fence(Ordering::SeqCst);

            let mut cons = VirtioConsole {
                notify_base: notify,
                notify_mult,
                rx_notify_off: rx_nof,
                tx_notify_off: tx_nof,
                rx_last_used: 0,
                tx_last_used: 0,
                qsize,
            };

            // Post RX buffers so host->guest input can arrive
            cons.rx_post_all();

            Some(cons)
        }
    }

    #[inline(always)]
    unsafe fn notify(&self, queue_index: u16, queue_notify_off: u16) {
        let addr = self.notify_base + (queue_notify_off as u64 * self.notify_mult as u64);
        // Correct per virtio-pci: write queue index (not 0).
        mmio_write_u16(addr, queue_index);
        fence(Ordering::SeqCst);
    }

    // ---------------- TX (prints) ----------------

    pub fn putc(&mut self, ch: u8) {
        let buf = [ch];
        self.write(&buf);
    }

    pub fn write(&mut self, bytes: &[u8]) {
        unsafe {
            if bytes.is_empty() {
                return;
            }

            // Copy to TX_BUF (truncate to RX_BUF_SZ just to bound runtime; adjust if needed)
            let n = core::cmp::min(bytes.len(), RX_BUF_SZ);
            for i in 0..n {
                write_volatile((TX_BUF + i as u64) as *mut u8, bytes[i]);
            }
            fence(Ordering::SeqCst);

            // Use a single descriptor index derived from tx_last_used (safe since we wait)
            let desc_idx = (self.tx_last_used as u16) % self.qsize;

            // desc[desc_idx] = TX buffer
            let desc_addr = TX_DESC + (desc_idx as u64) * 16;
            mmio_write_u64(desc_addr + 0, TX_BUF);
            mmio_write_u32(desc_addr + 8, n as u32);
            mmio_write_u16(desc_addr + 12, 0); // device reads only
            mmio_write_u16(desc_addr + 14, 0);
            fence(Ordering::SeqCst);

            // push desc_idx into avail ring
            let avail_idx_ptr = (TX_AVAIL + 2) as *mut u16;
            let avail_idx = read_volatile(avail_idx_ptr as *const u16);
            let ring_entry = (TX_AVAIL + 4 + ((avail_idx % self.qsize) as u64) * 2) as *mut u16;
            write_volatile(ring_entry, desc_idx);
            fence(Ordering::SeqCst);
            write_volatile(avail_idx_ptr, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // notify queue 1
            self.notify(1, self.tx_notify_off);

            // wait for used idx to advance (polled)
            let used_idx_ptr = (TX_USED + 2) as *const u16;
            for _ in 0..10_000_000u64 {
                fence(Ordering::SeqCst);
                let used_idx = read_volatile(used_idx_ptr);
                if used_idx != self.tx_last_used {
                    self.tx_last_used = used_idx;
                    return;
                }
                core::hint::spin_loop();
            }
            // If it times out, continue (debug prints best-effort)
        }
    }

    // Optional: implement core::fmt::Write so you can use write_fmt!/format_args!
    pub fn write_str(&mut self, s: &str) {
        self.write(s.as_bytes());
    }

    // ---------------- RX (optional input) ----------------

    fn rx_post_all(&mut self) {
        unsafe {
            // Post one buffer per descriptor slot
            for i in 0..self.qsize {
                let buf_addr = RX_BUFS + (i as u64) * (RX_BUF_SZ as u64);

                let desc_addr = RX_DESC + (i as u64) * 16;
                mmio_write_u64(desc_addr + 0, buf_addr);
                mmio_write_u32(desc_addr + 8, RX_BUF_SZ as u32);
                mmio_write_u16(desc_addr + 12, VRING_DESC_F_WRITE);
                mmio_write_u16(desc_addr + 14, 0);
            }
            fence(Ordering::SeqCst);

            // Fill avail ring with all descriptors
            let avail_idx_ptr = (RX_AVAIL + 2) as *mut u16;
            write_volatile(avail_idx_ptr, 0);
            for i in 0..self.qsize {
                let ring_entry = (RX_AVAIL + 4 + (i as u64) * 2) as *mut u16;
                write_volatile(ring_entry, i);
            }
            fence(Ordering::SeqCst);
            write_volatile(avail_idx_ptr, self.qsize);
            fence(Ordering::SeqCst);

            // notify queue 0
            self.notify(0, self.rx_notify_off);
        }
    }

    /// Poll for one received chunk. Returns number of bytes copied into `out`.
    /// Non-blocking: returns 0 if no input available.
    pub fn poll_read(&mut self, out: &mut [u8]) -> usize {
        unsafe {
            let used_idx_ptr = (RX_USED + 2) as *const u16;
            let used_idx = read_volatile(used_idx_ptr);
            if used_idx == self.rx_last_used {
                return 0;
            }

            // Read used element at (rx_last_used % qsize)
            let elem_idx = (self.rx_last_used % self.qsize) as u64;
            let used_elem_addr = RX_USED + 4 + elem_idx * 8;
            let id = mmio_read_u32(used_elem_addr + 0) as u16;
            let len = mmio_read_u32(used_elem_addr + 4) as usize;

            self.rx_last_used = used_idx;

            let n = core::cmp::min(len, out.len());
            let buf_addr = RX_BUFS + (id as u64) * (RX_BUF_SZ as u64);
            for i in 0..n {
                out[i] = read_volatile((buf_addr + i as u64) as *const u8);
            }

            // Repost the same descriptor id to avail ring
            let avail_idx_ptr = (RX_AVAIL + 2) as *mut u16;
            let avail_idx = read_volatile(avail_idx_ptr as *const u16);
            let ring_entry = (RX_AVAIL + 4 + ((avail_idx % self.qsize) as u64) * 2) as *mut u16;
            write_volatile(ring_entry, id);
            fence(Ordering::SeqCst);
            write_volatile(avail_idx_ptr, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            self.notify(0, self.rx_notify_off);

            n
        }
    }
}

unsafe fn setup_queue(
    common_cfg: u64,
    queue_index: u16,
    desired_size: u16,
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
) -> Option<(u16, u16)> {
    // Select queue
    mmio_write_u16(common_cfg + VIRTIO_PCI_COMMON_Q_SELECT as u64, queue_index);
    fence(Ordering::SeqCst);

    let max = mmio_read_u16(common_cfg + VIRTIO_PCI_COMMON_Q_SIZE as u64);
    if max == 0 {
        return None;
    }
    let qsz = core::cmp::min(max, desired_size);

    mmio_write_u16(common_cfg + VIRTIO_PCI_COMMON_Q_SIZE as u64, qsz);
    fence(Ordering::SeqCst);

    mmio_write_u32(common_cfg + VIRTIO_PCI_COMMON_Q_DESCLO as u64, desc_addr as u32);
    mmio_write_u32(common_cfg + VIRTIO_PCI_COMMON_Q_DESCHI as u64, (desc_addr >> 32) as u32);

    mmio_write_u32(common_cfg + VIRTIO_PCI_COMMON_Q_AVAILLO as u64, avail_addr as u32);
    mmio_write_u32(common_cfg + VIRTIO_PCI_COMMON_Q_AVAILHI as u64, (avail_addr >> 32) as u32);

    mmio_write_u32(common_cfg + VIRTIO_PCI_COMMON_Q_USEDLO as u64, used_addr as u32);
    mmio_write_u32(common_cfg + VIRTIO_PCI_COMMON_Q_USEDHI as u64, (used_addr >> 32) as u32);

    let notify_off = mmio_read_u16(common_cfg + VIRTIO_PCI_COMMON_Q_NOFF as u64);

    mmio_write_u16(common_cfg + VIRTIO_PCI_COMMON_Q_ENABLE as u64, 1);
    fence(Ordering::SeqCst);

    Some((notify_off, qsz))
}

// -------------------------- Discovery --------------------------

pub fn find_virtio_console() -> Option<VirtioConsole> {
    for &ecam in ECAM_ADDRESSES {
        unsafe {
            let vendor = read_volatile(ecam as *const u16);
            if vendor == 0 || vendor == 0xFFFF {
                continue;
            }
        }
        for bus in 0..4u8 {
            for dev in 0..32u8 {
                if let Some(c) = VirtioConsole::try_new(ecam, bus, dev) {
                    return Some(c);
                }
            }
        }
    }
    None
}

// -------------------------- Optional fmt::Write glue --------------------------

impl core::fmt::Write for VirtioConsole {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write(s.as_bytes());
        Ok(())
    }
}

// -------------------------- Global console instance --------------------------

static mut CONSOLE: Option<VirtioConsole> = None;

/// Initialize the virtio console. Call early in boot.
pub fn console_init() {
    unsafe {
        CONSOLE = find_virtio_console();
    }
}

/// Check if console is available
pub fn console_available() -> bool {
    unsafe { CONSOLE.is_some() }
}

/// Write a single byte
pub fn putc(b: u8) {
    unsafe {
        if let Some(c) = CONSOLE.as_mut() {
            c.putc(b);
        }
    }
}

/// Write a string
pub fn puts(s: &str) {
    unsafe {
        if let Some(c) = CONSOLE.as_mut() {
            c.write_str(s);
        }
    }
}

/// Write bytes
pub fn write_bytes(bytes: &[u8]) {
    unsafe {
        if let Some(c) = CONSOLE.as_mut() {
            c.write(bytes);
        }
    }
}

/// Print a hex value (useful for debugging addresses/values)
pub fn print_hex(x: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let shift = (15 - i) * 4;
        buf[i + 2] = HEX[((x >> shift) & 0xF) as usize];
    }
    write_bytes(&buf);
}

/// Print a decimal value
pub fn print_dec(mut x: u64) {
    if x == 0 {
        putc(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while x > 0 {
        buf[i] = b'0' + (x % 10) as u8;
        x /= 10;
        i += 1;
    }
    // Reverse
    while i > 0 {
        i -= 1;
        putc(buf[i]);
    }
}

/// Print with newline
pub fn println(s: &str) {
    puts(s);
    putc(b'\n');
}
