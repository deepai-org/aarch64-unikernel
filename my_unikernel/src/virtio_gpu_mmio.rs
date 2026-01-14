// Virtio GPU MMIO driver for HVF VMM
// Uses virtio-mmio transport instead of PCI

use core::ptr::{read_volatile, write_volatile};

// Device address (must match hvf_vmm.swift VIRTIO_GPU_BASE)
const VIRTIO_GPU_BASE: usize = 0x0a00_0000;

// Virtio MMIO register offsets
const VIRTIO_MMIO_MAGIC_VALUE: usize = 0x000;
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
const VIRTIO_MMIO_QUEUE_AVAIL_LOW: usize = 0x090;
const VIRTIO_MMIO_QUEUE_AVAIL_HIGH: usize = 0x094;
const VIRTIO_MMIO_QUEUE_USED_LOW: usize = 0x0a0;
const VIRTIO_MMIO_QUEUE_USED_HIGH: usize = 0x0a4;
const VIRTIO_MMIO_CONFIG: usize = 0x100;

// Virtio status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
const VIRTIO_STATUS_DRIVER: u32 = 2;
const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
const VIRTIO_STATUS_FEATURES_OK: u32 = 8;

// Virtio descriptor flags
const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

// GPU commands
const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;

// GPU responses
const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;

// Pixel formats
const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;

// Queue size
const QUEUE_SIZE: usize = 16;

// Virtio ring descriptor
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

// Available ring
#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
}

// Used element
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

// Used ring
#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; QUEUE_SIZE],
}

// GPU control header
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioGpuCtrlHdr {
    cmd_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    padding: u32,
}

// Display info response
#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioGpuDisplayOne {
    r_x: u32,
    r_y: u32,
    r_width: u32,
    r_height: u32,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
struct VirtioGpuRespDisplayInfo {
    hdr: VirtioGpuCtrlHdr,
    pmodes: [VirtioGpuDisplayOne; 16],
}

// Resource create 2D
#[repr(C)]
struct VirtioGpuResourceCreate2d {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

// Set scanout
#[repr(C)]
struct VirtioGpuSetScanout {
    hdr: VirtioGpuCtrlHdr,
    r_x: u32,
    r_y: u32,
    r_width: u32,
    r_height: u32,
    scanout_id: u32,
    resource_id: u32,
}

// Transfer to host 2D
#[repr(C)]
struct VirtioGpuTransferToHost2d {
    hdr: VirtioGpuCtrlHdr,
    r_x: u32,
    r_y: u32,
    r_width: u32,
    r_height: u32,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

// Resource flush
#[repr(C)]
struct VirtioGpuResourceFlush {
    hdr: VirtioGpuCtrlHdr,
    r_x: u32,
    r_y: u32,
    r_width: u32,
    r_height: u32,
    resource_id: u32,
    padding: u32,
}

// Attach backing
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
    entry: VirtioGpuMemEntry,
}

// Static buffers for virtqueue (must be aligned)
#[repr(align(4096))]
struct QueueBuffers {
    descs: [VirtqDesc; QUEUE_SIZE],
    avail: VirtqAvail,
    _padding: [u8; 2048],
    used: VirtqUsed,
}

static mut QUEUE_BUFFERS: QueueBuffers = QueueBuffers {
    descs: [VirtqDesc { addr: 0, len: 0, flags: 0, next: 0 }; QUEUE_SIZE],
    avail: VirtqAvail { flags: 0, idx: 0, ring: [0; QUEUE_SIZE] },
    _padding: [0; 2048],
    used: VirtqUsed { flags: 0, idx: 0, ring: [VirtqUsedElem { id: 0, len: 0 }; QUEUE_SIZE] },
};

// Command/response buffers
#[repr(align(4096))]
struct CmdBuffers {
    cmd: [u8; 4096],
    resp: [u8; 4096],
}

static mut CMD_BUFFERS: CmdBuffers = CmdBuffers {
    cmd: [0; 4096],
    resp: [0; 4096],
};

// Framebuffer
#[repr(align(4096))]
struct Framebuffer {
    data: [u32; 800 * 600],
}

static mut FRAMEBUFFER: Framebuffer = Framebuffer {
    data: [0; 800 * 600],
};

pub struct VirtioGpuMmio {
    base: usize,
    width: u32,
    height: u32,
    resource_id: u32,
    avail_idx: u16,
    last_used_idx: u16,
}

impl VirtioGpuMmio {
    fn read32(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, val: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, val) }
    }

    pub fn try_new() -> Option<Self> {
        use crate::puts;
        use crate::print_hex;

        let base = VIRTIO_GPU_BASE;

        // Check magic
        let magic = unsafe { read_volatile(base as *const u32) };
        puts("GPU magic: ");
        print_hex(magic as u64);
        puts("\n");
        if magic != 0x74726976 {
            puts("Bad magic\n");
            return None;
        }

        // Check version (must be 2 for modern)
        let version = unsafe { read_volatile((base + VIRTIO_MMIO_VERSION) as *const u32) };
        puts("GPU version: ");
        print_hex(version as u64);
        puts("\n");
        if version != 2 {
            puts("Bad version\n");
            return None;
        }

        // Check device ID (16 = GPU)
        let device_id = unsafe { read_volatile((base + VIRTIO_MMIO_DEVICE_ID) as *const u32) };
        puts("GPU device ID: ");
        print_hex(device_id as u64);
        puts("\n");
        if device_id != 16 {
            puts("Bad device ID\n");
            return None;
        }

        let mut gpu = VirtioGpuMmio {
            base,
            width: 0,
            height: 0,
            resource_id: 1,
            avail_idx: 0,
            last_used_idx: 0,
        };

        // Reset device
        gpu.write32(VIRTIO_MMIO_STATUS, 0);
        puts("GPU reset\n");

        // Set ACKNOWLEDGE
        gpu.write32(VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);

        // Set DRIVER
        gpu.write32(VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

        // Read features (we don't need any special features)
        gpu.write32(VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0);
        let _features = gpu.read32(VIRTIO_MMIO_DEVICE_FEATURES);

        // Accept features
        gpu.write32(VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0);
        gpu.write32(VIRTIO_MMIO_DRIVER_FEATURES, 0);

        // Set FEATURES_OK
        gpu.write32(VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK);

        // Setup controlq (queue 0)
        gpu.write32(VIRTIO_MMIO_QUEUE_SEL, 0);

        let max_queue_size = gpu.read32(VIRTIO_MMIO_QUEUE_NUM_MAX);
        puts("Max queue size: ");
        print_hex(max_queue_size as u64);
        puts("\n");
        if max_queue_size < QUEUE_SIZE as u32 {
            puts("Queue too small\n");
            return None;
        }

        gpu.write32(VIRTIO_MMIO_QUEUE_NUM, QUEUE_SIZE as u32);

        // Set queue addresses
        unsafe {
            let desc_addr = &QUEUE_BUFFERS.descs as *const _ as u64;
            let avail_addr = &QUEUE_BUFFERS.avail as *const _ as u64;
            let used_addr = &QUEUE_BUFFERS.used as *const _ as u64;

            puts("Desc addr: ");
            print_hex(desc_addr);
            puts("\n");

            gpu.write32(VIRTIO_MMIO_QUEUE_DESC_LOW, desc_addr as u32);
            gpu.write32(VIRTIO_MMIO_QUEUE_DESC_HIGH, (desc_addr >> 32) as u32);
            gpu.write32(VIRTIO_MMIO_QUEUE_AVAIL_LOW, avail_addr as u32);
            gpu.write32(VIRTIO_MMIO_QUEUE_AVAIL_HIGH, (avail_addr >> 32) as u32);
            gpu.write32(VIRTIO_MMIO_QUEUE_USED_LOW, used_addr as u32);
            gpu.write32(VIRTIO_MMIO_QUEUE_USED_HIGH, (used_addr >> 32) as u32);
        }

        // Mark queue ready
        gpu.write32(VIRTIO_MMIO_QUEUE_READY, 1);
        puts("Queue ready\n");

        // Set DRIVER_OK
        gpu.write32(VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK);
        puts("Device initialized\n");

        Some(gpu)
    }

    fn send_command(&mut self, cmd: &[u8], resp_len: usize) -> bool {
        use crate::puts;
        use crate::print_hex;

        puts("send_command: len=");
        print_hex(cmd.len() as u64);
        puts("\n");

        unsafe {
            // Copy command to buffer
            let cmd_addr = CMD_BUFFERS.cmd.as_ptr() as u64;
            let resp_addr = CMD_BUFFERS.resp.as_ptr() as u64;

            puts("cmd_addr: ");
            print_hex(cmd_addr);
            puts(" resp_addr: ");
            print_hex(resp_addr);
            puts("\n");

            for (i, &b) in cmd.iter().enumerate() {
                CMD_BUFFERS.cmd[i] = b;
            }

            // Clear response
            for i in 0..resp_len {
                CMD_BUFFERS.resp[i] = 0;
            }

            let desc_idx = (self.avail_idx % QUEUE_SIZE as u16) as usize;
            let resp_idx = ((self.avail_idx + 1) % QUEUE_SIZE as u16) as usize;

            puts("desc_idx: ");
            print_hex(desc_idx as u64);
            puts(" resp_idx: ");
            print_hex(resp_idx as u64);
            puts("\n");

            // Setup descriptors
            QUEUE_BUFFERS.descs[desc_idx] = VirtqDesc {
                addr: cmd_addr,
                len: cmd.len() as u32,
                flags: VRING_DESC_F_NEXT,
                next: resp_idx as u16,
            };
            QUEUE_BUFFERS.descs[resp_idx] = VirtqDesc {
                addr: resp_addr,
                len: resp_len as u32,
                flags: VRING_DESC_F_WRITE,
                next: 0,
            };

            // Add to available ring
            let avail_ring_idx = (self.avail_idx % QUEUE_SIZE as u16) as usize;
            QUEUE_BUFFERS.avail.ring[avail_ring_idx] = desc_idx as u16;

            // Memory barrier
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

            QUEUE_BUFFERS.avail.idx = self.avail_idx.wrapping_add(1);
            self.avail_idx = self.avail_idx.wrapping_add(2);

            puts("avail.idx: ");
            print_hex(QUEUE_BUFFERS.avail.idx as u64);
            puts("\n");

            // Notify device
            puts("Notifying queue 0\n");
            self.write32(VIRTIO_MMIO_QUEUE_NOTIFY, 0);

            // Wait for completion
            puts("Waiting for response...\n");
            for i in 0..100000 {
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                if QUEUE_BUFFERS.used.idx != self.last_used_idx {
                    puts("Response received! used.idx: ");
                    print_hex(QUEUE_BUFFERS.used.idx as u64);
                    puts("\n");
                    self.last_used_idx = QUEUE_BUFFERS.used.idx;
                    return true;
                }
                if i == 99999 {
                    puts("Timeout! last_used: ");
                    print_hex(self.last_used_idx as u64);
                    puts(" used.idx: ");
                    print_hex(QUEUE_BUFFERS.used.idx as u64);
                    puts("\n");
                }
            }
        }
        false
    }

    pub fn init_display(&mut self) -> bool {
        // Get display info
        let cmd = VirtioGpuCtrlHdr {
            cmd_type: VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            padding: 0,
        };

        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&cmd as *const _ as *const u8, core::mem::size_of::<VirtioGpuCtrlHdr>())
        };

        if !self.send_command(cmd_bytes, core::mem::size_of::<VirtioGpuRespDisplayInfo>()) {
            return false;
        }

        // Parse response
        unsafe {
            let resp = &*(CMD_BUFFERS.resp.as_ptr() as *const VirtioGpuRespDisplayInfo);
            if resp.hdr.cmd_type != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
                return false;
            }

            self.width = resp.pmodes[0].r_width;
            self.height = resp.pmodes[0].r_height;

            if self.width == 0 || self.height == 0 {
                self.width = 800;
                self.height = 600;
            }
        }

        // Create 2D resource
        let create = VirtioGpuResourceCreate2d {
            hdr: VirtioGpuCtrlHdr {
                cmd_type: VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            resource_id: self.resource_id,
            format: VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
            width: self.width,
            height: self.height,
        };

        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&create as *const _ as *const u8, core::mem::size_of::<VirtioGpuResourceCreate2d>())
        };

        if !self.send_command(cmd_bytes, 24) {
            return false;
        }

        // Attach backing storage
        let attach = VirtioGpuResourceAttachBacking {
            hdr: VirtioGpuCtrlHdr {
                cmd_type: VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            resource_id: self.resource_id,
            nr_entries: 1,
            entry: VirtioGpuMemEntry {
                addr: unsafe { FRAMEBUFFER.data.as_ptr() as u64 },
                length: (self.width * self.height * 4) as u32,
                padding: 0,
            },
        };

        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&attach as *const _ as *const u8, core::mem::size_of::<VirtioGpuResourceAttachBacking>())
        };

        if !self.send_command(cmd_bytes, 24) {
            return false;
        }

        // Set scanout
        let scanout = VirtioGpuSetScanout {
            hdr: VirtioGpuCtrlHdr {
                cmd_type: VIRTIO_GPU_CMD_SET_SCANOUT,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r_x: 0,
            r_y: 0,
            r_width: self.width,
            r_height: self.height,
            scanout_id: 0,
            resource_id: self.resource_id,
        };

        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&scanout as *const _ as *const u8, core::mem::size_of::<VirtioGpuSetScanout>())
        };

        self.send_command(cmd_bytes, 24)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn fill(&mut self, color: u32) {
        unsafe {
            let pixels = self.width as usize * self.height as usize;
            for i in 0..pixels.min(FRAMEBUFFER.data.len()) {
                FRAMEBUFFER.data[i] = color;
            }
        }
    }

    pub fn draw_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        unsafe {
            for dy in 0..h {
                let py = y + dy;
                if py >= self.height { continue; }
                for dx in 0..w {
                    let px = x + dx;
                    if px >= self.width { continue; }
                    let idx = (py * self.width + px) as usize;
                    if idx < FRAMEBUFFER.data.len() {
                        FRAMEBUFFER.data[idx] = color;
                    }
                }
            }
        }
    }

    pub fn flush(&mut self) {
        // Transfer to host
        let transfer = VirtioGpuTransferToHost2d {
            hdr: VirtioGpuCtrlHdr {
                cmd_type: VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r_x: 0,
            r_y: 0,
            r_width: self.width,
            r_height: self.height,
            offset: 0,
            resource_id: self.resource_id,
            padding: 0,
        };

        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&transfer as *const _ as *const u8, core::mem::size_of::<VirtioGpuTransferToHost2d>())
        };

        self.send_command(cmd_bytes, 24);

        // Flush
        let flush_cmd = VirtioGpuResourceFlush {
            hdr: VirtioGpuCtrlHdr {
                cmd_type: VIRTIO_GPU_CMD_RESOURCE_FLUSH,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r_x: 0,
            r_y: 0,
            r_width: self.width,
            r_height: self.height,
            resource_id: self.resource_id,
            padding: 0,
        };

        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(&flush_cmd as *const _ as *const u8, core::mem::size_of::<VirtioGpuResourceFlush>())
        };

        self.send_command(cmd_bytes, 24);
    }
}
