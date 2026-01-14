//! Minimal virtio MMIO driver for console output
//!
//! Based on virtio 1.0 specification (legacy mode for simplicity)

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// Virtio MMIO register offsets
const VIRTIO_MMIO_MAGIC: usize = 0x000;
const VIRTIO_MMIO_VERSION: usize = 0x004;
const VIRTIO_MMIO_DEVICE_ID: usize = 0x008;
const VIRTIO_MMIO_VENDOR_ID: usize = 0x00c;
const VIRTIO_MMIO_DEVICE_FEATURES: usize = 0x010;
const VIRTIO_MMIO_DEVICE_FEATURES_SEL: usize = 0x014;
const VIRTIO_MMIO_DRIVER_FEATURES: usize = 0x020;
const VIRTIO_MMIO_DRIVER_FEATURES_SEL: usize = 0x024;
const VIRTIO_MMIO_QUEUE_SEL: usize = 0x030;
const VIRTIO_MMIO_QUEUE_NUM_MAX: usize = 0x034;
const VIRTIO_MMIO_QUEUE_NUM: usize = 0x038;
const VIRTIO_MMIO_QUEUE_READY: usize = 0x044;
const VIRTIO_MMIO_QUEUE_NOTIFY: usize = 0x050;
const VIRTIO_MMIO_INTERRUPT_STATUS: usize = 0x060;
const VIRTIO_MMIO_INTERRUPT_ACK: usize = 0x064;
const VIRTIO_MMIO_STATUS: usize = 0x070;
const VIRTIO_MMIO_QUEUE_DESC_LOW: usize = 0x080;
const VIRTIO_MMIO_QUEUE_DESC_HIGH: usize = 0x084;
const VIRTIO_MMIO_QUEUE_DRIVER_LOW: usize = 0x090;
const VIRTIO_MMIO_QUEUE_DRIVER_HIGH: usize = 0x094;
const VIRTIO_MMIO_QUEUE_DEVICE_LOW: usize = 0x0a0;
const VIRTIO_MMIO_QUEUE_DEVICE_HIGH: usize = 0x0a4;

// Virtio device status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
const VIRTIO_STATUS_DRIVER: u32 = 2;
const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
const VIRTIO_STATUS_FEATURES_OK: u32 = 8;

// Virtio device IDs
const VIRTIO_DEV_CONSOLE: u32 = 3;

// Magic value
const VIRTIO_MAGIC: u32 = 0x74726976; // "virt"

// Descriptor flags
const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

// Queue size (must be power of 2)
const QUEUE_SIZE: u16 = 16;

// Vring descriptor
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VringDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

// Vring available ring
#[repr(C)]
struct VringAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE as usize],
}

// Vring used element
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VringUsedElem {
    id: u32,
    len: u32,
}

// Vring used ring
#[repr(C)]
struct VringUsed {
    flags: u16,
    idx: u16,
    ring: [VringUsedElem; QUEUE_SIZE as usize],
}

// Static buffers for virtqueues (aligned to 4096)
#[repr(C, align(4096))]
struct VirtqueueBuffers {
    descs: [VringDesc; QUEUE_SIZE as usize],
    avail: VringAvail,
    _pad: [u8; 4096 - core::mem::size_of::<[VringDesc; QUEUE_SIZE as usize]>() - core::mem::size_of::<VringAvail>()],
    used: VringUsed,
}

// Transmit buffer
#[repr(C, align(16))]
struct TxBuffer {
    data: [u8; 256],
}

static mut TX_QUEUE: VirtqueueBuffers = VirtqueueBuffers {
    descs: [VringDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE as usize],
    avail: VringAvail { flags: 0, idx: 0, ring: [0; QUEUE_SIZE as usize] },
    _pad: [0; 4096 - core::mem::size_of::<[VringDesc; QUEUE_SIZE as usize]>() - core::mem::size_of::<VringAvail>()],
    used: VringUsed { flags: 0, idx: 0, ring: [VringUsedElem { id: 0, len: 0 }; QUEUE_SIZE as usize] },
};

static mut TX_BUFFER: TxBuffer = TxBuffer { data: [0; 256] };
static mut TX_IDX: u16 = 0;
static mut LAST_USED_IDX: u16 = 0;

pub struct VirtioConsole {
    base: usize,
}

impl VirtioConsole {
    /// Try to find and initialize a virtio console at the given base address
    pub fn try_new(base: usize) -> Option<Self> {
        unsafe {
            // Check magic
            let magic = read_volatile((base + VIRTIO_MMIO_MAGIC) as *const u32);
            if magic != VIRTIO_MAGIC {
                return None;
            }

            // Check version (1 = legacy, 2 = modern)
            let version = read_volatile((base + VIRTIO_MMIO_VERSION) as *const u32);
            if version != 1 && version != 2 {
                return None;
            }

            // Check device ID
            let device_id = read_volatile((base + VIRTIO_MMIO_DEVICE_ID) as *const u32);
            if device_id != VIRTIO_DEV_CONSOLE {
                return None;
            }

            // Initialize the device
            // 1. Reset
            write_volatile((base + VIRTIO_MMIO_STATUS) as *mut u32, 0);
            fence(Ordering::SeqCst);

            // 2. Acknowledge
            write_volatile((base + VIRTIO_MMIO_STATUS) as *mut u32, VIRTIO_STATUS_ACKNOWLEDGE);
            fence(Ordering::SeqCst);

            // 3. Driver
            write_volatile((base + VIRTIO_MMIO_STATUS) as *mut u32,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);
            fence(Ordering::SeqCst);

            // 4. Read features (we don't need any special features)
            write_volatile((base + VIRTIO_MMIO_DEVICE_FEATURES_SEL) as *mut u32, 0);
            let _features = read_volatile((base + VIRTIO_MMIO_DEVICE_FEATURES) as *const u32);

            // 5. Write features (accept none)
            write_volatile((base + VIRTIO_MMIO_DRIVER_FEATURES_SEL) as *mut u32, 0);
            write_volatile((base + VIRTIO_MMIO_DRIVER_FEATURES) as *mut u32, 0);

            // 6. Features OK
            write_volatile((base + VIRTIO_MMIO_STATUS) as *mut u32,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK);
            fence(Ordering::SeqCst);

            // Check features OK
            let status = read_volatile((base + VIRTIO_MMIO_STATUS) as *const u32);
            if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
                return None;
            }

            // 7. Set up transmit queue (queue 1 for console)
            // Queue 0 = receiveq, Queue 1 = transmitq
            write_volatile((base + VIRTIO_MMIO_QUEUE_SEL) as *mut u32, 1);
            fence(Ordering::SeqCst);

            let queue_max = read_volatile((base + VIRTIO_MMIO_QUEUE_NUM_MAX) as *const u32);
            if queue_max == 0 || queue_max < QUEUE_SIZE as u32 {
                return None;
            }

            // Set queue size
            write_volatile((base + VIRTIO_MMIO_QUEUE_NUM) as *mut u32, QUEUE_SIZE as u32);

            // Set queue addresses
            let desc_addr = &TX_QUEUE.descs as *const _ as u64;
            let avail_addr = &TX_QUEUE.avail as *const _ as u64;
            let used_addr = &TX_QUEUE.used as *const _ as u64;

            write_volatile((base + VIRTIO_MMIO_QUEUE_DESC_LOW) as *mut u32, desc_addr as u32);
            write_volatile((base + VIRTIO_MMIO_QUEUE_DESC_HIGH) as *mut u32, (desc_addr >> 32) as u32);
            write_volatile((base + VIRTIO_MMIO_QUEUE_DRIVER_LOW) as *mut u32, avail_addr as u32);
            write_volatile((base + VIRTIO_MMIO_QUEUE_DRIVER_HIGH) as *mut u32, (avail_addr >> 32) as u32);
            write_volatile((base + VIRTIO_MMIO_QUEUE_DEVICE_LOW) as *mut u32, used_addr as u32);
            write_volatile((base + VIRTIO_MMIO_QUEUE_DEVICE_HIGH) as *mut u32, (used_addr >> 32) as u32);

            // Enable queue
            write_volatile((base + VIRTIO_MMIO_QUEUE_READY) as *mut u32, 1);
            fence(Ordering::SeqCst);

            // 8. Driver OK
            write_volatile((base + VIRTIO_MMIO_STATUS) as *mut u32,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER |
                VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK);
            fence(Ordering::SeqCst);

            Some(VirtioConsole { base })
        }
    }

    /// Probe common virtio MMIO addresses to find a console
    pub fn probe() -> Option<Self> {
        // Common virtio MMIO addresses used by various VMMs
        const PROBE_ADDRS: &[usize] = &[
            // Standard QEMU/Firecracker (0x200 stride)
            0x0a000000, 0x0a000200, 0x0a000400, 0x0a000600,
            0x0a000800, 0x0a000a00, 0x0a000c00, 0x0a000e00,
            0x0a001000, 0x0a001200, 0x0a001400, 0x0a001600,
            0x0a003c00, 0x0a003e00,
            // 0x1000 stride variants
            0x0a000000, 0x0a001000, 0x0a002000, 0x0a003000,
            0x0a004000, 0x0a005000, 0x0a006000, 0x0a007000,
            // Different base addresses
            0x0b000000, 0x0b000200, 0x0b001000,
            0x0c000000, 0x0c000200, 0x0c001000,
            0x0d000000, 0x0d000200, 0x0d001000,
            0x0e000000, 0x0e000200, 0x0e001000,
            0x0f000000, 0x0f000200, 0x0f001000,
            // High memory
            0x10000000, 0x10000200, 0x10001000,
            0x10010000, 0x10020000, 0x10030000,
            // Very high memory
            0x20000000, 0x30000000, 0x40010000,
        ];

        for &addr in PROBE_ADDRS {
            if let Some(console) = Self::try_new(addr) {
                return Some(console);
            }
        }
        None
    }

    /// Write a single character
    pub fn putc(&self, c: u8) {
        unsafe {
            // Wait for space in the queue
            fence(Ordering::SeqCst);

            let idx = TX_IDX % QUEUE_SIZE;

            // Set up descriptor
            TX_BUFFER.data[0] = c;
            TX_QUEUE.descs[idx as usize] = VringDesc {
                addr: TX_BUFFER.data.as_ptr() as u64,
                len: 1,
                flags: 0,
                next: 0,
            };

            // Add to available ring
            let avail_idx = TX_QUEUE.avail.idx;
            TX_QUEUE.avail.ring[(avail_idx % QUEUE_SIZE) as usize] = idx;
            fence(Ordering::SeqCst);
            TX_QUEUE.avail.idx = avail_idx.wrapping_add(1);
            fence(Ordering::SeqCst);

            // Notify device (queue 1 = transmitq)
            write_volatile((self.base + VIRTIO_MMIO_QUEUE_NOTIFY) as *mut u32, 1);
            fence(Ordering::SeqCst);

            TX_IDX = TX_IDX.wrapping_add(1);

            // Simple busy wait for completion
            for _ in 0..10000 {
                fence(Ordering::SeqCst);
                if TX_QUEUE.used.idx != LAST_USED_IDX {
                    LAST_USED_IDX = TX_QUEUE.used.idx;
                    break;
                }
                core::hint::spin_loop();
            }
        }
    }

    /// Write a string
    pub fn puts(&self, s: &str) {
        for b in s.bytes() {
            if b == b'\n' {
                self.putc(b'\r');
            }
            self.putc(b);
        }
    }
}
