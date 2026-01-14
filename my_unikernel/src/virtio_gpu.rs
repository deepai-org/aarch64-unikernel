//! Virtio GPU driver for simple framebuffer output
//!
//! Implements basic virtio-gpu protocol to display graphics

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

// PCI config space offsets
const PCI_VENDOR_ID: usize = 0x00;
const PCI_DEVICE_ID: usize = 0x02;
const PCI_COMMAND: usize = 0x04;
const PCI_STATUS: usize = 0x06;
const PCI_BAR0: usize = 0x10;
const PCI_CAP_PTR: usize = 0x34;

const PCI_COMMAND_MEMORY: u16 = 0x02;
const PCI_COMMAND_BUS_MASTER: u16 = 0x04;

const VIRTIO_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_GPU_DEVICE_ID: u16 = 0x1050;  // Modern virtio-gpu

// Virtio PCI capability types
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;

// Virtio status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;

// Common config offsets
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

// GPU command types
const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;

// GPU response types
const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
// Error responses start at 0x1200
const VIRTIO_GPU_RESP_ERR_UNSPEC: u32 = 0x1200;

// Response buffer address (must match send_cmd)
const RESP_ADDR: u64 = 0x8000_4000;

// GPU formats
const VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM: u32 = 1;  // Try this for Apple Silicon
const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 3;

// Hardcoded framebuffer address in VZ RAM (0x70000000 + 32MB offset)
const HARDCODED_FB_ADDR: u64 = 0x7200_0000;

const QUEUE_SIZE: u16 = 16;

// Vring structures
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VringDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

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
struct Virtqueue {
    descs: [VringDesc; QUEUE_SIZE as usize],
    avail: VringAvail,
    _pad: [u8; 2048],
    used: VringUsed,
}

// GPU command/response headers
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioGpuCtrlHdr {
    cmd_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    padding: u32,
}

#[repr(C)]
struct VirtioGpuRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct VirtioGpuDisplayOne {
    r: VirtioGpuRect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
struct VirtioGpuRespDisplayInfo {
    hdr: VirtioGpuCtrlHdr,
    pmodes: [VirtioGpuDisplayOne; 16],
}

#[repr(C)]
struct VirtioGpuResourceCreate2d {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct VirtioGpuSetScanout {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
struct VirtioGpuMemEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
struct VirtioGpuResourceAttachBacking {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    nr_entries: u32,
}

#[repr(C)]
struct VirtioGpuTransferToHost2d {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
struct VirtioGpuResourceFlush {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    resource_id: u32,
    padding: u32,
}

// Static buffers
static mut GPU_QUEUE: Virtqueue = Virtqueue {
    descs: [VringDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE as usize],
    avail: VringAvail { flags: 0, idx: 0, ring: [0; QUEUE_SIZE as usize] },
    _pad: [0; 2048],
    used: VringUsed { flags: 0, idx: 0, ring: [VringUsedElem { id: 0, len: 0 }; QUEUE_SIZE as usize] },
};

// Command/response buffers
#[repr(C, align(64))]
struct CmdBuffer {
    data: [u8; 4096],
}

static mut CMD_BUF: CmdBuffer = CmdBuffer { data: [0; 4096] };
static mut RESP_BUF: CmdBuffer = CmdBuffer { data: [0; 4096] };

// Framebuffer - 1280x720 @ 32bpp = ~3.5MB
const FB_WIDTH: u32 = 1280;
const FB_HEIGHT: u32 = 720;
const FB_BPP: u32 = 4;
const FB_SIZE: usize = (FB_WIDTH * FB_HEIGHT * FB_BPP) as usize;

#[repr(C, align(4096))]
struct Framebuffer {
    pixels: [u32; (FB_WIDTH * FB_HEIGHT) as usize],
}

static mut FRAMEBUFFER: Framebuffer = Framebuffer {
    pixels: [0; (FB_WIDTH * FB_HEIGHT) as usize],
};

static mut QUEUE_IDX: u16 = 0;
static mut LAST_USED_IDX: u16 = 0;

pub struct VirtioGpu {
    notify_base: u64,           // Pre-computed: BAR address + notify_cfg offset
    notify_off_multiplier: u32,
    queue_notify_off: u16,
    width: u32,
    height: u32,
}

impl VirtioGpu {
    /// Try to create GPU with retries - polls until device appears or timeout
    pub fn try_new_with_retry(ecam_base: u64, bus: u8, device: u8, max_retries: u32) -> Option<Self> {
        for retry in 0..max_retries {
            // Check if device is present (valid vendor ID)
            unsafe {
                let config_base = ecam_base
                    + ((bus as u64) << 20)
                    + ((device as u64) << 15);
                let header = core::ptr::read_volatile(config_base as *const u32);
                let vendor_id = (header & 0xFFFF) as u16;

                // 0xFFFF means device not ready yet
                if vendor_id == 0xFFFF || vendor_id == 0 {
                    // Wait a bit and retry
                    for _ in 0..100_000u64 {
                        core::hint::spin_loop();
                    }
                    continue;
                }
            }

            // Device appears present, try full init
            if let Some(gpu) = Self::try_new(ecam_base, bus, device) {
                return Some(gpu);
            }

            // Init failed, wait and retry
            for _ in 0..100_000u64 {
                core::hint::spin_loop();
            }
        }
        None
    }

    pub fn try_new(ecam_base: u64, bus: u8, device: u8) -> Option<Self> {
        unsafe {
            let config_base = ecam_base
                + ((bus as u64) << 20)
                + ((device as u64) << 15);

            // SANITY CHECK: Read header as u32 (safer for MMIO)
            let header = read_volatile(config_base as *const u32);
            let vendor_id = (header & 0xFFFF) as u16;
            let device_id = ((header >> 16) & 0xFFFF) as u16;

            // Skip non-virtio devices (no crash - just return None)
            if vendor_id != VIRTIO_VENDOR_ID {
                return None;
            }

            // Accept GPU device (0x1050) or transitional (0x1040)
            if device_id != 0x1050 && device_id != 0x1040 && device_id != 0x1010 {
                return None;
            }

            // CRITICAL: Enable Command Register BEFORE reading BARs
            // VZ starts devices with decoding DISABLED
            let cmd_ptr = (config_base + PCI_COMMAND as u64) as *mut u16;
            let cmd = read_volatile(cmd_ptr);
            // Enable Memory Space (bit 1) + Bus Master (bit 2) + IO (bit 0)
            write_volatile(cmd_ptr, cmd | 0x07);
            fence(Ordering::SeqCst);

            // VZ WORKAROUND: BARs are not programmed by VZ - we must do it ourselves!
            // Based on Linux dmesg, GPU BAR0 should be at 0x50008000
            // Program BAR0 with this address if it's currently zero
            let bar0_ptr = (config_base + 0x10) as *mut u32;
            let bar0_val = read_volatile(bar0_ptr);
            if bar0_val == 0 {
                // GPU BAR0 address from Linux: 50008000-5000bfff (16KB)
                // This is a 32-bit Memory BAR (bits 0-3 are type flags, we set to 0)
                write_volatile(bar0_ptr, 0x50008000);
                fence(Ordering::SeqCst);
            }

            // BAR CHECK removed - we now program BAR0 ourselves

            // Parse capabilities (BARs are read dynamically based on capability bar index)
            let status = read_volatile((config_base + PCI_STATUS as u64) as *const u16);
            if (status & 0x10) == 0 {
                // No capabilities - fail
                core::arch::asm!("brk #4");
                return None;
            }

            // Helper to read BAR address by index (handles 32-bit and 64-bit BARs)
            let read_bar = |bar_idx: u8| -> u64 {
                if bar_idx > 5 {
                    return 0;
                }
                let bar_reg = config_base + 0x10 + ((bar_idx as u64) * 4);
                let bar_low = read_volatile(bar_reg as *const u32);

                // Check if 64-bit BAR (type field bits 1-2 == 0b10)
                if (bar_low & 0x6) == 0x4 && bar_idx < 5 {
                    let bar_high = read_volatile((bar_reg + 4) as *const u32);
                    ((bar_high as u64) << 32) | ((bar_low & !0xF) as u64)
                } else {
                    (bar_low & !0xF) as u64
                }
            };

            let mut cap_ptr = read_volatile((config_base + PCI_CAP_PTR as u64) as *const u8);

            // Store the actual base address (BAR value + offset) for each config type
            let mut common_base: Option<u64> = None;
            let mut notify_base: Option<u64> = None;
            let mut notify_mult: u32 = 0;

            while cap_ptr != 0 {
                let cap_id = read_volatile((config_base + cap_ptr as u64) as *const u8);
                let cap_next = read_volatile((config_base + cap_ptr as u64 + 1) as *const u8);

                if cap_id == 0x09 {
                    // Virtio PCI capability structure:
                    // +0x00: cap_vndr (0x09)
                    // +0x01: cap_next
                    // +0x02: cap_len
                    // +0x03: cfg_type (1=Common, 2=Notify, etc.)
                    // +0x04: bar (which BAR holds this config)
                    // +0x08: offset (within that BAR)
                    // +0x0C: length
                    let cfg_type = read_volatile((config_base + cap_ptr as u64 + 3) as *const u8);
                    let bar_idx = read_volatile((config_base + cap_ptr as u64 + 4) as *const u8);
                    let offset = read_volatile((config_base + cap_ptr as u64 + 8) as *const u32);

                    // Look up the actual BAR address using the index from the capability
                    let bar_addr = read_bar(bar_idx);

                    if bar_addr != 0 {
                        let final_addr = bar_addr + (offset as u64);

                        match cfg_type {
                            VIRTIO_PCI_CAP_COMMON_CFG => {
                                common_base = Some(final_addr);
                            }
                            VIRTIO_PCI_CAP_NOTIFY_CFG => {
                                notify_base = Some(final_addr);
                                notify_mult = read_volatile((config_base + cap_ptr as u64 + 16) as *const u32);
                            }
                            _ => {}
                        }
                    }
                }
                cap_ptr = cap_next;
            }

            let common_cfg = match common_base {
                Some(v) => v,
                None => {
                    return None;
                }
            };
            let notify_cfg = match notify_base {
                Some(v) => v,
                None => {
                    return None;
                }
            };

            // GHOST TEST: Verify common_cfg points to real hardware
            // num_queues is at offset 0x10 in common_cfg
            let num_queues = read_volatile((common_cfg + 0x10) as *const u16);
            if num_queues == 0 {
                // We're reading unmapped memory - BAR address is wrong
                return None;
            }

            // Initialize device
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8, 0);
            fence(Ordering::SeqCst);

            write_volatile((common_cfg + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8, VIRTIO_STATUS_ACKNOWLEDGE);
            fence(Ordering::SeqCst);

            write_volatile((common_cfg + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);
            fence(Ordering::SeqCst);

            // Features negotiation (VirtIO 1.0+ strict requirement)
            // CRITICAL: Only accept features we actually implement!
            // Accepting unknown features (like RING_PACKED, ACCESS_PLATFORM) breaks the driver.

            // --- BANK 0 (Bits 0-31) ---
            // Accept ZERO features in bank 0 - we don't implement any optional features
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_GFSELECT as u64) as *mut u32, 0);
            fence(Ordering::SeqCst);
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_GF as u64) as *mut u32, 0);  // No features!
            fence(Ordering::SeqCst);

            // --- BANK 1 (Bits 32-63) ---
            // ONLY accept VIRTIO_F_VERSION_1 (bit 0 of bank 1 = bit 32)
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_GFSELECT as u64) as *mut u32, 1);
            fence(Ordering::SeqCst);
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_GF as u64) as *mut u32, 1);  // Only VERSION_1!
            fence(Ordering::SeqCst);

            // Step 5: Write FEATURES_OK
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK);
            fence(Ordering::SeqCst);

            // Step 6 (CRITICAL): Re-read status to verify FEATURES_OK is still set
            let status = read_volatile((common_cfg + VIRTIO_PCI_COMMON_STATUS as u64) as *const u8);
            if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
                return None;
            }

            // Setup queue 0 (controlq)
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_SELECT as u64) as *mut u16, 0);
            fence(Ordering::SeqCst);

            let queue_size_max = read_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *const u16);
            if queue_size_max == 0 {
                return None;
            }

            let actual_size = if queue_size_max < QUEUE_SIZE { queue_size_max } else { QUEUE_SIZE };
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_SIZE as u64) as *mut u16, actual_size);

            // Use fixed RAM addresses for queue structures (0x80000000 region)
            // This ensures the device can access them via DMA
            const QUEUE_RAM_BASE: u64 = 0x8000_0000;
            const DESC_OFFSET: u64 = 0;
            const AVAIL_OFFSET: u64 = 0x1000;  // 4KB aligned
            const USED_OFFSET: u64 = 0x2000;   // 4KB aligned

            let desc_addr = QUEUE_RAM_BASE + DESC_OFFSET;
            let avail_addr = QUEUE_RAM_BASE + AVAIL_OFFSET;
            let used_addr = QUEUE_RAM_BASE + USED_OFFSET;

            // Zero initialize the queue memory
            for i in 0..0x3000u64 {
                write_volatile((QUEUE_RAM_BASE + i) as *mut u8, 0);
            }

            write_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_DESCLO as u64) as *mut u32, desc_addr as u32);
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_DESCHI as u64) as *mut u32, (desc_addr >> 32) as u32);
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_AVAILLO as u64) as *mut u32, avail_addr as u32);
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_AVAILHI as u64) as *mut u32, (avail_addr >> 32) as u32);
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_USEDLO as u64) as *mut u32, used_addr as u32);
            write_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_USEDHI as u64) as *mut u32, (used_addr >> 32) as u32);

            let queue_notify_off = read_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_NOFF as u64) as *const u16);

            write_volatile((common_cfg + VIRTIO_PCI_COMMON_Q_ENABLE as u64) as *mut u16, 1);
            fence(Ordering::SeqCst);

            write_volatile((common_cfg + VIRTIO_PCI_COMMON_STATUS as u64) as *mut u8,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK);
            fence(Ordering::SeqCst);

            // Small delay after DRIVER_OK to let device stabilize
            for _ in 0..100_000u64 {
                core::hint::spin_loop();
            }

            Some(VirtioGpu {
                notify_base: notify_cfg,
                notify_off_multiplier: notify_mult,
                queue_notify_off,
                width: FB_WIDTH,
                height: FB_HEIGHT,
            })
        }
    }

    /// Returns the response type from the command, or 0 on timeout
    fn send_cmd(&self, cmd: &[u8], resp_len: usize) -> u32 {
        unsafe {
            fence(Ordering::SeqCst);

            // Fixed RAM addresses for queue structures
            const QUEUE_RAM_BASE: u64 = 0x8000_0000;
            const DESC_BASE: u64 = QUEUE_RAM_BASE;
            const AVAIL_BASE: u64 = QUEUE_RAM_BASE + 0x1000;
            const USED_BASE: u64 = QUEUE_RAM_BASE + 0x2000;
            // Command/response buffers in fixed RAM
            const CMD_BASE: u64 = QUEUE_RAM_BASE + 0x3000;
            const RESP_BASE: u64 = QUEUE_RAM_BASE + 0x4000;

            // Copy command to fixed RAM buffer
            for (i, &b) in cmd.iter().enumerate() {
                write_volatile((CMD_BASE + i as u64) as *mut u8, b);
            }
            fence(Ordering::SeqCst);

            let idx = QUEUE_IDX % QUEUE_SIZE;
            let next_idx = (idx + 1) % QUEUE_SIZE;

            // Write descriptor 0: command (device reads)
            let desc0_addr = DESC_BASE + (idx as u64) * 16;
            write_volatile((desc0_addr + 0) as *mut u64, CMD_BASE);         // addr
            write_volatile((desc0_addr + 8) as *mut u32, cmd.len() as u32); // len
            write_volatile((desc0_addr + 12) as *mut u16, VRING_DESC_F_NEXT); // flags
            write_volatile((desc0_addr + 14) as *mut u16, next_idx);        // next

            // Write descriptor 1: response (device writes)
            let desc1_addr = DESC_BASE + (next_idx as u64) * 16;
            write_volatile((desc1_addr + 0) as *mut u64, RESP_BASE);        // addr
            write_volatile((desc1_addr + 8) as *mut u32, resp_len as u32);  // len
            write_volatile((desc1_addr + 12) as *mut u16, VRING_DESC_F_WRITE); // flags
            write_volatile((desc1_addr + 14) as *mut u16, 0);               // next

            // Add to available ring
            // avail ring: flags(2) + idx(2) + ring[N](2*N)
            let avail_idx_ptr = (AVAIL_BASE + 2) as *mut u16;
            let avail_idx = read_volatile(avail_idx_ptr as *const u16);
            let ring_entry_ptr = (AVAIL_BASE + 4 + ((avail_idx % QUEUE_SIZE) as u64) * 2) as *mut u16;
            write_volatile(ring_entry_ptr, idx);
            fence(Ordering::SeqCst);
            write_volatile(avail_idx_ptr, avail_idx.wrapping_add(1));
            fence(Ordering::SeqCst);

            // Notify - DEBUG: crash with notify addr in x0 if multiplier looks wrong
            let notify_addr = self.notify_base
                + (self.queue_notify_off as u64 * self.notify_off_multiplier as u64);

            // DEBUG disabled - notify addr debugging
            // x0 = notify_addr, x1 = notify_base, x2 = multiplier, x3 = queue_notify_off

            write_volatile(notify_addr as *mut u16, 0);
            fence(Ordering::SeqCst);

            QUEUE_IDX = QUEUE_IDX.wrapping_add(2);

            // Wait for response - read from fixed USED ring address
            // used ring: flags(2) + idx(2) + ring[N](8*N)
            let used_idx_ptr = (USED_BASE + 2) as *const u16;
            for _ in 0..50_000_000u64 {
                fence(Ordering::SeqCst);
                let used_idx = read_volatile(used_idx_ptr);
                if used_idx != LAST_USED_IDX {
                    LAST_USED_IDX = used_idx;
                    // Read and return response type from response buffer
                    let resp_hdr = RESP_BASE as *const VirtioGpuCtrlHdr;
                    return read_volatile(&(*resp_hdr).cmd_type);
                }
                core::hint::spin_loop();
            }
            // TIMEOUT
            LAST_USED_IDX = LAST_USED_IDX.wrapping_add(1);
            0  // Return 0 to indicate timeout
        }
    }

    pub fn init_display(&mut self) -> bool {
        // Helper macro to check response - crashes with specific code if error
        // x0 = response code, x1 = cmd_id
        macro_rules! check_resp {
            ($resp:expr, $expected:expr, $cmd_id:expr) => {
                unsafe {
                    let r = $resp;
                    let e = $expected;
                    let id = $cmd_id as u64;
                    if r == 0 {
                        // Timeout - brk #0xD0
                        core::arch::asm!("mov x1, {0}", "brk #0xD0", in(reg) id);
                    } else if r >= VIRTIO_GPU_RESP_ERR_UNSPEC {
                        // Error response - brk #0xE0, x0=resp, x1=cmd_id
                        core::arch::asm!("mov x0, {0}", "mov x1, {1}", "brk #0xE0",
                            in(reg) r as u64, in(reg) id);
                    } else if r != e {
                        // Unexpected - brk #0xF0
                        core::arch::asm!("mov x0, {0}", "mov x1, {1}", "brk #0xF0",
                            in(reg) r as u64, in(reg) id);
                    }
                }
            };
        }

        // 1. Get display info
        let hdr = VirtioGpuCtrlHdr {
            cmd_type: VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
        };
        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&hdr as *const _ as *const u8, core::mem::size_of::<VirtioGpuCtrlHdr>())
        };
        let resp = self.send_cmd(cmd_bytes, core::mem::size_of::<VirtioGpuRespDisplayInfo>());
        check_resp!(resp, VIRTIO_GPU_RESP_OK_DISPLAY_INFO, 1);

        // Check dimensions from response (but use hardcoded anyway)
        unsafe {
            let disp_resp = &*(RESP_ADDR as *const VirtioGpuRespDisplayInfo);
            if disp_resp.pmodes[0].enabled != 0 {
                self.width = disp_resp.pmodes[0].r.width;
                self.height = disp_resp.pmodes[0].r.height;
            }
        }

        // 2. Create 2D resource
        let create = VirtioGpuResourceCreate2d {
            hdr: VirtioGpuCtrlHdr {
                cmd_type: VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            resource_id: 1,
            format: VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
            width: 1280,
            height: 720,
        };
        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&create as *const _ as *const u8, core::mem::size_of::<VirtioGpuResourceCreate2d>())
        };
        let resp = self.send_cmd(cmd_bytes, core::mem::size_of::<VirtioGpuCtrlHdr>());
        check_resp!(resp, VIRTIO_GPU_RESP_OK_NODATA, 2);

        // 3. Attach backing (framebuffer memory)
        #[repr(C)]
        struct AttachCmd {
            hdr: VirtioGpuResourceAttachBacking,
            entry: VirtioGpuMemEntry,
        }

        let attach = AttachCmd {
            hdr: VirtioGpuResourceAttachBacking {
                hdr: VirtioGpuCtrlHdr {
                    cmd_type: VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
                    flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
                },
                resource_id: 1,
                nr_entries: 1,
            },
            entry: VirtioGpuMemEntry {
                addr: HARDCODED_FB_ADDR,
                length: 1280 * 720 * 4,
                padding: 0,
            },
        };
        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&attach as *const _ as *const u8, core::mem::size_of::<AttachCmd>())
        };
        let resp = self.send_cmd(cmd_bytes, core::mem::size_of::<VirtioGpuCtrlHdr>());
        check_resp!(resp, VIRTIO_GPU_RESP_OK_NODATA, 3);

        // 4. Set scanout
        let scanout = VirtioGpuSetScanout {
            hdr: VirtioGpuCtrlHdr {
                cmd_type: VIRTIO_GPU_CMD_SET_SCANOUT,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            r: VirtioGpuRect { x: 0, y: 0, width: 1280, height: 720 },
            scanout_id: 0,
            resource_id: 1,
        };
        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&scanout as *const _ as *const u8, core::mem::size_of::<VirtioGpuSetScanout>())
        };
        let resp = self.send_cmd(cmd_bytes, core::mem::size_of::<VirtioGpuCtrlHdr>());
        check_resp!(resp, VIRTIO_GPU_RESP_OK_NODATA, 4);

        true  // All commands succeeded
    }

    pub fn fill(&self, color: u32) {
        const PIXELS: usize = 1280 * 720;
        unsafe {
            let ptr = HARDCODED_FB_ADDR as *mut u32;
            for i in 0..PIXELS {
                ptr.add(i).write_volatile(color);
            }
        }
    }

    pub fn draw_rect(&self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        const WIDTH: u32 = 1280;
        const HEIGHT: u32 = 720;
        unsafe {
            let ptr = HARDCODED_FB_ADDR as *mut u32;
            for dy in 0..h {
                for dx in 0..w {
                    let px = x + dx;
                    let py = y + dy;
                    if px < WIDTH && py < HEIGHT {
                        let idx = (py * WIDTH + px) as usize;
                        ptr.add(idx).write_volatile(color);
                    }
                }
            }
        }
    }

    pub fn flush(&self) {
        // Transfer to host - hardcoded dimensions
        let transfer = VirtioGpuTransferToHost2d {
            hdr: VirtioGpuCtrlHdr {
                cmd_type: VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            r: VirtioGpuRect { x: 0, y: 0, width: 1280, height: 720 },
            offset: 0,
            resource_id: 1,
            padding: 0,
        };

        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(
                &transfer as *const _ as *const u8,
                core::mem::size_of::<VirtioGpuTransferToHost2d>()
            )
        };

        self.send_cmd(cmd_bytes, core::mem::size_of::<VirtioGpuCtrlHdr>());

        // Flush - hardcoded dimensions
        let flush = VirtioGpuResourceFlush {
            hdr: VirtioGpuCtrlHdr {
                cmd_type: VIRTIO_GPU_CMD_RESOURCE_FLUSH,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            r: VirtioGpuRect { x: 0, y: 0, width: 1280, height: 720 },
            resource_id: 1,
            padding: 0,
        };

        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(
                &flush as *const _ as *const u8,
                core::mem::size_of::<VirtioGpuResourceFlush>()
            )
        };

        self.send_cmd(cmd_bytes, core::mem::size_of::<VirtioGpuCtrlHdr>());
    }

    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }

    /// Compute a simple checksum of the framebuffer for testing
    /// Returns (checksum, non_zero_pixels) for verification
    pub fn framebuffer_checksum(&self) -> (u32, u32) {
        const PIXELS: usize = 1280 * 720;
        let mut checksum: u32 = 0;
        let mut non_zero: u32 = 0;

        unsafe {
            let ptr = HARDCODED_FB_ADDR as *const u32;
            for i in 0..PIXELS {
                let pixel = ptr.add(i).read_volatile();
                // Simple checksum: XOR with position-mixed value
                checksum = checksum.wrapping_add(pixel ^ (i as u32).wrapping_mul(0x9e3779b9));
                if pixel != 0 {
                    non_zero += 1;
                }
            }
        }
        (checksum, non_zero)
    }

    /// Sample specific pixels for test verification
    /// Returns array of pixel values at test coordinates
    pub fn sample_test_pixels(&self) -> [u32; 5] {
        const WIDTH: usize = 1280;
        let test_coords = [
            (100, 150),   // First colored box area
            (330, 150),   // Second colored box area
            (560, 150),   // Third colored box area
            (640, 20),    // Title bar area
            (640, 500),   // Background area
        ];

        let mut samples = [0u32; 5];
        unsafe {
            let ptr = HARDCODED_FB_ADDR as *const u32;
            for (i, (x, y)) in test_coords.iter().enumerate() {
                let idx = y * WIDTH + x;
                samples[i] = ptr.add(idx).read_volatile();
            }
        }
        samples
    }
}

// ECAM addresses to try
// NOTE: 0x40000000 is RAM_BASE - do NOT include it!
// VZ on Apple Silicon may use high addresses (above 4GB)
const ECAM_ADDRESSES: &[u64] = &[
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
    // High addresses
    0x1_00000000,
    0x2_00000000,
    0x4_00000000,
    0x5_00000000,
    0x6_00000000,
    0x8_00000000,
    0x10_00000000,
];

pub fn find_virtio_gpu() -> Option<VirtioGpu> {
    for &ecam in ECAM_ADDRESSES {
        unsafe {
            let vendor = read_volatile(ecam as *const u16);
            if vendor == 0 || vendor == 0xffff {
                continue;
            }
        }
        for bus in 0..4u8 {
            for device in 0..32u8 {
                if let Some(gpu) = VirtioGpu::try_new(ecam, bus, device) {
                    return Some(gpu);
                }
            }
        }
    }
    None
}

pub fn find_virtio_gpu_at_ecam(ecam: u64) -> Option<VirtioGpu> {
    for bus in 0..4u8 {
        for device in 0..32u8 {
            if let Some(gpu) = VirtioGpu::try_new(ecam, bus, device) {
                return Some(gpu);
            }
        }
    }
    None
}

/// Scan and report all PCI devices found at ECAM
pub fn scan_pci_devices(ecam: u64) {
    unsafe {
        for bus in 0..2u8 {
            for device in 0..32u8 {
                let config_base = ecam + ((bus as u64) << 20) + ((device as u64) << 15);
                let vendor_id = read_volatile(config_base as *const u16);
                if vendor_id != 0 && vendor_id != 0xFFFF {
                    let device_id = read_volatile((config_base + 2) as *const u16);
                    // Report this device - caller will print
                    crate::puts("PCI ");
                    crate::putc(b'0' + bus);
                    crate::puts(":");
                    crate::print_hex(device as u64);
                    crate::puts(" = ");
                    crate::print_hex(vendor_id as u64);
                    crate::puts(":");
                    crate::print_hex(device_id as u64);
                    crate::puts("\n");
                }
            }
        }
    }
}
