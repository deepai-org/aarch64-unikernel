//! VirtIO Network device driver
//!
//! Basic network driver with TX/RX queues.
//! Device IDs: 0x1000 (transitional), 0x1041 (modern)

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

const VIRTIO_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_NET_TRANSITIONAL: u16 = 0x1000;
const VIRTIO_NET_MODERN: u16 = 0x1041;

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

// Network feature bits
const VIRTIO_NET_F_MAC: u32 = 1 << 5;

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

// Descriptor flags
const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

const QUEUE_SIZE: u16 = 8;
const MTU: usize = 1514; // Ethernet MTU

/// VirtIO net header (prepended to every packet)
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VirtioNetHeader {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    // num_buffers only present with VIRTIO_NET_F_MRG_RXBUF
}

const NET_HDR_SIZE: usize = core::mem::size_of::<VirtioNetHeader>();

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

// RX queue buffers
static mut RX_QUEUE_DESCS: [VringDesc; QUEUE_SIZE as usize] =
    [VringDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE as usize];
static mut RX_QUEUE_AVAIL: VringAvail = VringAvail {
    flags: 0, idx: 0, ring: [0; QUEUE_SIZE as usize]
};
static mut RX_QUEUE_USED: VringUsed = VringUsed {
    flags: 0, idx: 0, ring: [VringUsedElem { id: 0, len: 0 }; QUEUE_SIZE as usize]
};

// TX queue buffers
static mut TX_QUEUE_DESCS: [VringDesc; QUEUE_SIZE as usize] =
    [VringDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE as usize];
static mut TX_QUEUE_AVAIL: VringAvail = VringAvail {
    flags: 0, idx: 0, ring: [0; QUEUE_SIZE as usize]
};
static mut TX_QUEUE_USED: VringUsed = VringUsed {
    flags: 0, idx: 0, ring: [VringUsedElem { id: 0, len: 0 }; QUEUE_SIZE as usize]
};

// Packet buffers
static mut RX_BUFFER: [u8; NET_HDR_SIZE + MTU] = [0; NET_HDR_SIZE + MTU];
static mut TX_BUFFER: [u8; NET_HDR_SIZE + MTU] = [0; NET_HDR_SIZE + MTU];

static mut RX_IDX: u16 = 0;
static mut TX_IDX: u16 = 0;
static mut RX_LAST_USED: u16 = 0;
static mut TX_LAST_USED: u16 = 0;

#[derive(Clone, Copy)]
struct VirtioCap {
    bar: u8,
    offset: u32,
    notify_off_multiplier: u32,
}

/// VirtIO Network driver
pub struct VirtioNet {
    bar0: u64,
    notify_offset: u32,
    notify_multiplier: u32,
    rx_notify_off: u16,
    tx_notify_off: u16,
    mac: [u8; 6],
}

impl VirtioNet {
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

        // Read features
        write_volatile((common_base + 0) as *mut u32, 0);
        let features = read_volatile((common_base + 4) as *const u32);

        // Accept MAC feature if available + VERSION_1
        let accepted = features & VIRTIO_NET_F_MAC;
        write_volatile((common_base + 8) as *mut u32, 0);
        write_volatile((common_base + 12) as *mut u32, accepted);
        write_volatile((common_base + 8) as *mut u32, 1);
        write_volatile((common_base + 12) as *mut u32, 1);
        fence(Ordering::SeqCst);

        // Features OK
        write_volatile((common_base + 20) as *mut u8, 0x0B);
        fence(Ordering::SeqCst);

        if (read_volatile((common_base + 20) as *const u8) & 0x08) == 0 {
            return None;
        }

        // Read MAC address from device config
        let mut mac = [0u8; 6];
        if device_base != 0 && (accepted & VIRTIO_NET_F_MAC) != 0 {
            for i in 0..6 {
                mac[i] = read_volatile((device_base + i as u64) as *const u8);
            }
        }

        // Setup RX queue (0)
        write_volatile((common_base + 22) as *mut u16, 0);
        fence(Ordering::SeqCst);

        let queue_size_max = read_volatile((common_base + 24) as *const u16);
        if queue_size_max == 0 { return None; }

        let actual_size = queue_size_max.min(QUEUE_SIZE);
        write_volatile((common_base + 24) as *mut u16, actual_size);

        let desc_addr = &raw const RX_QUEUE_DESCS as u64;
        let avail_addr = &raw const RX_QUEUE_AVAIL as u64;
        let used_addr = &raw const RX_QUEUE_USED as u64;

        write_volatile((common_base + 32) as *mut u32, desc_addr as u32);
        write_volatile((common_base + 36) as *mut u32, (desc_addr >> 32) as u32);
        write_volatile((common_base + 40) as *mut u32, avail_addr as u32);
        write_volatile((common_base + 44) as *mut u32, (avail_addr >> 32) as u32);
        write_volatile((common_base + 48) as *mut u32, used_addr as u32);
        write_volatile((common_base + 52) as *mut u32, (used_addr >> 32) as u32);

        let rx_notify_off = read_volatile((common_base + 30) as *const u16);
        write_volatile((common_base + 28) as *mut u16, 1);
        fence(Ordering::SeqCst);

        // Setup TX queue (1)
        write_volatile((common_base + 22) as *mut u16, 1);
        fence(Ordering::SeqCst);

        let queue_size_max = read_volatile((common_base + 24) as *const u16);
        if queue_size_max == 0 { return None; }

        let actual_size = queue_size_max.min(QUEUE_SIZE);
        write_volatile((common_base + 24) as *mut u16, actual_size);

        let desc_addr = &raw const TX_QUEUE_DESCS as u64;
        let avail_addr = &raw const TX_QUEUE_AVAIL as u64;
        let used_addr = &raw const TX_QUEUE_USED as u64;

        write_volatile((common_base + 32) as *mut u32, desc_addr as u32);
        write_volatile((common_base + 36) as *mut u32, (desc_addr >> 32) as u32);
        write_volatile((common_base + 40) as *mut u32, avail_addr as u32);
        write_volatile((common_base + 44) as *mut u32, (avail_addr >> 32) as u32);
        write_volatile((common_base + 48) as *mut u32, used_addr as u32);
        write_volatile((common_base + 52) as *mut u32, (used_addr >> 32) as u32);

        let tx_notify_off = read_volatile((common_base + 30) as *const u16);
        write_volatile((common_base + 28) as *mut u16, 1);
        fence(Ordering::SeqCst);

        // Driver OK
        write_volatile((common_base + 20) as *mut u8, 0x0F);
        fence(Ordering::SeqCst);

        // Post initial RX buffer
        Self::post_rx_buffer(notify_base, 0, modern.notify_mult, rx_notify_off);

        Some(VirtioNet {
            bar0: notify_base,
            notify_offset: 0,
            notify_multiplier: modern.notify_mult,
            rx_notify_off,
            tx_notify_off,
            mac,
        })
    }

    /// Find and initialize network device on PCI bus (legacy)
    pub fn find(ecam: u64) -> Option<Self> {
        for dev in 0u8..32 {
            let config_base = ecam + ((dev as u64) << 15);

            unsafe {
                let vendor = read_volatile(config_base as *const u16);
                if vendor != VIRTIO_VENDOR_ID {
                    continue;
                }

                let device_id = read_volatile((config_base + 2) as *const u16);
                if device_id != VIRTIO_NET_TRANSITIONAL && device_id != VIRTIO_NET_MODERN {
                    continue;
                }

                if let Some(net) = Self::try_new(config_base, dev) {
                    return Some(net);
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

            // 4. Read features - check for MAC feature
            write_volatile((common_base + VIRTIO_PCI_COMMON_DFSELECT as u64) as *mut u32, 0);
            let features = read_volatile((common_base + VIRTIO_PCI_COMMON_DF as u64) as *const u32);

            // 5. Accept MAC feature if available, and MUST set VERSION_1
            let accepted_features = features & VIRTIO_NET_F_MAC;
            write_volatile((common_base + VIRTIO_PCI_COMMON_GFSELECT as u64) as *mut u32, 0);
            write_volatile((common_base + VIRTIO_PCI_COMMON_GF as u64) as *mut u32, accepted_features);
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

            // Read MAC address if device config available
            let mut mac = [0u8; 6];
            if let Some(dev_cfg) = device_cap {
                if dev_cfg.bar == 0 && (accepted_features & VIRTIO_NET_F_MAC) != 0 {
                    let dev_base = bar0 + dev_cfg.offset as u64;
                    for i in 0..6 {
                        mac[i] = read_volatile((dev_base + i as u64) as *const u8);
                    }
                }
            }

            // 7. Setup RX queue (queue 0)
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SELECT as u64) as *mut u16, 0);
            fence(Ordering::SeqCst);

            let queue_size_max = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *const u16);
            if queue_size_max == 0 {
                return None;
            }

            let actual_size = queue_size_max.min(QUEUE_SIZE);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *mut u16, actual_size);

            let desc_addr = &raw const RX_QUEUE_DESCS as u64;
            let avail_addr = &raw const RX_QUEUE_AVAIL as u64;
            let used_addr = &raw const RX_QUEUE_USED as u64;

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCLO as u64) as *mut u32, desc_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCHI as u64) as *mut u32, (desc_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILLO as u64) as *mut u32, avail_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILHI as u64) as *mut u32, (avail_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDLO as u64) as *mut u32, used_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDHI as u64) as *mut u32, (used_addr >> 32) as u32);

            let rx_notify_off = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_NOFF as u64) as *const u16);

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_ENABLE as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            // 8. Setup TX queue (queue 1)
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SELECT as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            let queue_size_max = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *const u16);
            if queue_size_max == 0 {
                return None;
            }

            let actual_size = queue_size_max.min(QUEUE_SIZE);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *mut u16, actual_size);

            let desc_addr = &raw const TX_QUEUE_DESCS as u64;
            let avail_addr = &raw const TX_QUEUE_AVAIL as u64;
            let used_addr = &raw const TX_QUEUE_USED as u64;

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCLO as u64) as *mut u32, desc_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_DESCHI as u64) as *mut u32, (desc_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILLO as u64) as *mut u32, avail_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_AVAILHI as u64) as *mut u32, (avail_addr >> 32) as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDLO as u64) as *mut u32, used_addr as u32);
            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_USEDHI as u64) as *mut u32, (used_addr >> 32) as u32);

            let tx_notify_off = read_volatile((common_base + VIRTIO_PCI_COMMON_Q_NOFF as u64) as *const u16);

            write_volatile((common_base + VIRTIO_PCI_COMMON_Q_ENABLE as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            // 9. Driver OK
            write_volatile(
                (common_base + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK,
            );
            fence(Ordering::SeqCst);

            // Post initial RX buffer
            Self::post_rx_buffer(bar0, notify.offset, notify.notify_off_multiplier, rx_notify_off);

            Some(VirtioNet {
                bar0,
                notify_offset: notify.offset,
                notify_multiplier: notify.notify_off_multiplier,
                rx_notify_off,
                tx_notify_off,
                mac,
            })
        }
    }

    fn post_rx_buffer(bar0: u64, notify_offset: u32, notify_multiplier: u32, rx_notify_off: u16) {
        unsafe {
            let idx = RX_IDX % QUEUE_SIZE;

            // RX buffer: device writes header + packet
            RX_QUEUE_DESCS[idx as usize] = VringDesc {
                addr: RX_BUFFER.as_ptr() as u64,
                len: (NET_HDR_SIZE + MTU) as u32,
                flags: VRING_DESC_F_WRITE,
                next: 0,
            };

            fence(Ordering::SeqCst);

            let avail_idx = read_volatile(&RX_QUEUE_AVAIL.idx);
            write_volatile(
                &raw mut RX_QUEUE_AVAIL.ring[(avail_idx % QUEUE_SIZE) as usize],
                idx,
            );
            fence(Ordering::SeqCst);
            write_volatile(&raw mut RX_QUEUE_AVAIL.idx, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Notify device
            let notify_addr = bar0
                + notify_offset as u64
                + (rx_notify_off as u64 * notify_multiplier as u64);
            write_volatile(notify_addr as *mut u16, 0);
            fence(Ordering::SeqCst);

            RX_IDX = RX_IDX.wrapping_add(1);
        }
    }

    /// Get MAC address
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Send a packet (without headers)
    pub fn send(&self, data: &[u8]) -> bool {
        if data.len() > MTU {
            return false;
        }

        unsafe {
            // Setup header (all zeros for basic transmit)
            let hdr = VirtioNetHeader::default();
            let hdr_bytes = core::slice::from_raw_parts(
                &hdr as *const VirtioNetHeader as *const u8,
                NET_HDR_SIZE
            );

            // Copy header
            for i in 0..NET_HDR_SIZE {
                TX_BUFFER[i] = hdr_bytes[i];
            }

            // Copy data
            for i in 0..data.len() {
                TX_BUFFER[NET_HDR_SIZE + i] = data[i];
            }

            let total_len = NET_HDR_SIZE + data.len();
            let idx = TX_IDX % QUEUE_SIZE;

            TX_QUEUE_DESCS[idx as usize] = VringDesc {
                addr: TX_BUFFER.as_ptr() as u64,
                len: total_len as u32,
                flags: 0,
                next: 0,
            };

            fence(Ordering::SeqCst);

            let avail_idx = read_volatile(&TX_QUEUE_AVAIL.idx);
            write_volatile(
                &raw mut TX_QUEUE_AVAIL.ring[(avail_idx % QUEUE_SIZE) as usize],
                idx,
            );
            fence(Ordering::SeqCst);
            write_volatile(&raw mut TX_QUEUE_AVAIL.idx, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Notify device
            let notify_addr = self.bar0
                + self.notify_offset as u64
                + (self.tx_notify_off as u64 * self.notify_multiplier as u64);
            write_volatile(notify_addr as *mut u16, 1);
            fence(Ordering::SeqCst);

            TX_IDX = TX_IDX.wrapping_add(1);

            // Wait for completion
            for _ in 0..100_000 {
                fence(Ordering::SeqCst);
                let used_idx = read_volatile(&TX_QUEUE_USED.idx);
                if used_idx != TX_LAST_USED {
                    TX_LAST_USED = used_idx;
                    return true;
                }
                core::hint::spin_loop();
            }

            false
        }
    }

    /// Try to receive a packet (returns length or 0 if no packet)
    pub fn recv(&self, buf: &mut [u8]) -> usize {
        unsafe {
            fence(Ordering::SeqCst);
            let used_idx = read_volatile(&RX_QUEUE_USED.idx);
            if used_idx == RX_LAST_USED {
                return 0;
            }

            let used_elem = read_volatile(
                &RX_QUEUE_USED.ring[(RX_LAST_USED % QUEUE_SIZE) as usize]
            );
            RX_LAST_USED = used_idx;

            let total_len = used_elem.len as usize;
            if total_len <= NET_HDR_SIZE {
                // Re-post buffer
                Self::post_rx_buffer(
                    self.bar0, self.notify_offset, self.notify_multiplier, self.rx_notify_off
                );
                return 0;
            }

            let data_len = total_len - NET_HDR_SIZE;
            let copy_len = data_len.min(buf.len());

            for i in 0..copy_len {
                buf[i] = read_volatile(&RX_BUFFER[NET_HDR_SIZE + i]);
            }

            // Re-post buffer
            Self::post_rx_buffer(
                self.bar0, self.notify_offset, self.notify_multiplier, self.rx_notify_off
            );

            copy_len
        }
    }

    /// Test network by sending a broadcast frame and checking for any response
    pub fn test_network(&self) -> NetTestResult {
        // Create a simple ARP request (broadcast)
        let mut arp_packet = [0u8; 42];

        // Ethernet header
        // Destination: broadcast FF:FF:FF:FF:FF:FF
        for i in 0..6 { arp_packet[i] = 0xFF; }
        // Source: our MAC
        for i in 0..6 { arp_packet[6 + i] = self.mac[i]; }
        // EtherType: ARP (0x0806)
        arp_packet[12] = 0x08;
        arp_packet[13] = 0x06;

        // ARP header
        arp_packet[14] = 0x00; arp_packet[15] = 0x01; // Hardware type: Ethernet
        arp_packet[16] = 0x08; arp_packet[17] = 0x00; // Protocol type: IPv4
        arp_packet[18] = 6;    // Hardware size
        arp_packet[19] = 4;    // Protocol size
        arp_packet[20] = 0x00; arp_packet[21] = 0x01; // Opcode: request

        // Sender MAC
        for i in 0..6 { arp_packet[22 + i] = self.mac[i]; }
        // Sender IP: 10.0.0.2
        arp_packet[28] = 10; arp_packet[29] = 0; arp_packet[30] = 0; arp_packet[31] = 2;

        // Target MAC: zeros (unknown)
        // Target IP: 10.0.0.1 (gateway)
        arp_packet[38] = 10; arp_packet[39] = 0; arp_packet[40] = 0; arp_packet[41] = 1;

        // Send the ARP request
        let send_ok = self.send(&arp_packet);

        // Brief wait for response
        let mut recv_buf = [0u8; 64];
        let mut received = false;
        for _ in 0..10000 {
            let len = self.recv(&mut recv_buf);
            if len > 0 {
                received = true;
                break;
            }
            core::hint::spin_loop();
        }

        NetTestResult {
            mac: self.mac,
            send_ok,
            received_response: received,
            init_ok: true,
        }
    }
}

/// Network test result
pub struct NetTestResult {
    pub mac: [u8; 6],
    pub send_ok: bool,
    pub received_response: bool,
    pub init_ok: bool,
}
