// Low-level VMM using Hypervisor.framework directly
// This gives us more control over memory layout and device emulation

import Foundation
import Hypervisor

// Memory configuration
let RAM_SIZE: UInt64 = 512 * 1024 * 1024  // 512 MB
let RAM_BASE: UInt64 = 0x7000_0000        // VZ-compatible (0x40000000 is PCI ECAM in VZ)

// PL011 UART at standard address (handled via MMIO traps, not mapped)
let PL011_BASE: UInt64 = 0x0900_0000
let PL011_SIZE: UInt64 = 0x1000

// Virtio-GPU MMIO device
let VIRTIO_GPU_BASE: UInt64 = 0x0a00_0000
let VIRTIO_GPU_SIZE: UInt64 = 0x1000

// Force unbuffered output
setbuf(stdout, nil)
setbuf(stderr, nil)

func log(_ msg: String) {
    fputs("\(msg)\n", stderr)
}

// MARK: - Virtio GPU Device

// Virtio MMIO register offsets
let VIRTIO_MMIO_MAGIC_VALUE: UInt64 = 0x000
let VIRTIO_MMIO_VERSION: UInt64 = 0x004
let VIRTIO_MMIO_DEVICE_ID: UInt64 = 0x008
let VIRTIO_MMIO_VENDOR_ID: UInt64 = 0x00c
let VIRTIO_MMIO_DEVICE_FEATURES: UInt64 = 0x010
let VIRTIO_MMIO_DEVICE_FEATURES_SEL: UInt64 = 0x014
let VIRTIO_MMIO_DRIVER_FEATURES: UInt64 = 0x020
let VIRTIO_MMIO_DRIVER_FEATURES_SEL: UInt64 = 0x024
let VIRTIO_MMIO_QUEUE_SEL: UInt64 = 0x030
let VIRTIO_MMIO_QUEUE_NUM_MAX: UInt64 = 0x034
let VIRTIO_MMIO_QUEUE_NUM: UInt64 = 0x038
let VIRTIO_MMIO_QUEUE_READY: UInt64 = 0x044
let VIRTIO_MMIO_QUEUE_NOTIFY: UInt64 = 0x050
let VIRTIO_MMIO_INTERRUPT_STATUS: UInt64 = 0x060
let VIRTIO_MMIO_INTERRUPT_ACK: UInt64 = 0x064
let VIRTIO_MMIO_STATUS: UInt64 = 0x070
let VIRTIO_MMIO_QUEUE_DESC_LOW: UInt64 = 0x080
let VIRTIO_MMIO_QUEUE_DESC_HIGH: UInt64 = 0x084
let VIRTIO_MMIO_QUEUE_AVAIL_LOW: UInt64 = 0x090
let VIRTIO_MMIO_QUEUE_AVAIL_HIGH: UInt64 = 0x094
let VIRTIO_MMIO_QUEUE_USED_LOW: UInt64 = 0x0a0
let VIRTIO_MMIO_QUEUE_USED_HIGH: UInt64 = 0x0a4
let VIRTIO_MMIO_CONFIG: UInt64 = 0x100

// GPU commands
let VIRTIO_GPU_CMD_GET_DISPLAY_INFO: UInt32 = 0x0100
let VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: UInt32 = 0x0101
let VIRTIO_GPU_CMD_SET_SCANOUT: UInt32 = 0x0103
let VIRTIO_GPU_CMD_RESOURCE_FLUSH: UInt32 = 0x0104
let VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: UInt32 = 0x0105
let VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: UInt32 = 0x0106

// GPU responses
let VIRTIO_GPU_RESP_OK_NODATA: UInt32 = 0x1100
let VIRTIO_GPU_RESP_OK_DISPLAY_INFO: UInt32 = 0x1101

// Virtio descriptor flags
let VRING_DESC_F_NEXT: UInt16 = 1
let VRING_DESC_F_WRITE: UInt16 = 2

// GPU resource structure
struct GpuResource {
    var id: UInt32 = 0
    var format: UInt32 = 0
    var width: UInt32 = 0
    var height: UInt32 = 0
    var backingAddr: UInt64 = 0
    var backingLength: UInt32 = 0
}

// Virtqueue state
struct VirtqueueState {
    var descAddr: UInt64 = 0
    var availAddr: UInt64 = 0
    var usedAddr: UInt64 = 0
    var num: UInt32 = 0
    var ready: Bool = false
    var lastAvailIdx: UInt16 = 0
}

class VirtioGpuDevice {
    // Device registers
    var deviceFeaturesSel: UInt32 = 0
    var driverFeatures: UInt32 = 0
    var driverFeaturesSel: UInt32 = 0
    var queueSel: UInt32 = 0
    var status: UInt32 = 0
    var interruptStatus: UInt32 = 0

    // Queues (0 = controlq, 1 = cursorq)
    var queues: [VirtqueueState] = [VirtqueueState(), VirtqueueState()]

    // GPU state
    var resources: [UInt32: GpuResource] = [:]
    var scanoutResourceId: UInt32 = 0
    var scanoutWidth: UInt32 = 800
    var scanoutHeight: UInt32 = 600

    // Framebuffer
    var framebuffer: [UInt8] = []
    var framebufferWidth: UInt32 = 0
    var framebufferHeight: UInt32 = 0
    var flushCount: Int = 0

    // Guest RAM access
    let ram: UnsafeMutableRawPointer
    let ramBase: UInt64

    init(ram: UnsafeMutableRawPointer, ramBase: UInt64) {
        self.ram = ram
        self.ramBase = ramBase
    }

    func guestToHost(_ guestAddr: UInt64) -> UnsafeMutableRawPointer? {
        if guestAddr >= ramBase && guestAddr < ramBase + RAM_SIZE {
            return ram.advanced(by: Int(guestAddr - ramBase))
        }
        return nil
    }

    func read32(_ offset: UInt64) -> UInt32 {
        switch offset {
        case VIRTIO_MMIO_MAGIC_VALUE:
            return 0x74726976  // "virt"
        case VIRTIO_MMIO_VERSION:
            return 2  // Modern virtio-mmio
        case VIRTIO_MMIO_DEVICE_ID:
            return 16  // GPU device
        case VIRTIO_MMIO_VENDOR_ID:
            return 0x554d4551  // "QEMU" for compatibility
        case VIRTIO_MMIO_DEVICE_FEATURES:
            if deviceFeaturesSel == 0 {
                return 0  // No special features for now
            }
            return 0
        case VIRTIO_MMIO_QUEUE_NUM_MAX:
            return 256  // Max queue size
        case VIRTIO_MMIO_QUEUE_READY:
            let q = Int(queueSel)
            return queues[q].ready ? 1 : 0
        case VIRTIO_MMIO_INTERRUPT_STATUS:
            return interruptStatus
        case VIRTIO_MMIO_STATUS:
            return status
        case VIRTIO_MMIO_CONFIG..<(VIRTIO_MMIO_CONFIG + 24):
            let configOffset = offset - VIRTIO_MMIO_CONFIG
            switch configOffset {
            case 0: return 0  // events_read
            case 4: return 0  // events_clear
            case 8: return 1  // num_scanouts
            default: return 0
            }
        default:
            return 0
        }
    }

    func write32(_ offset: UInt64, _ value: UInt32) {
        log("GPU write: offset=0x\(String(offset, radix: 16)) value=0x\(String(value, radix: 16))")
        switch offset {
        case VIRTIO_MMIO_DEVICE_FEATURES_SEL:
            deviceFeaturesSel = value
        case VIRTIO_MMIO_DRIVER_FEATURES:
            driverFeatures = value
        case VIRTIO_MMIO_DRIVER_FEATURES_SEL:
            driverFeaturesSel = value
        case VIRTIO_MMIO_QUEUE_SEL:
            queueSel = value
            log("GPU: Queue select = \(value)")
        case VIRTIO_MMIO_QUEUE_NUM:
            let q = Int(queueSel)
            if q < queues.count {
                queues[q].num = value
                log("GPU: Queue \(q) num = \(value)")
            }
        case VIRTIO_MMIO_QUEUE_READY:
            let q = Int(queueSel)
            if q < queues.count {
                queues[q].ready = value != 0
                if value != 0 {
                    log("GPU: Queue \(q) ready (num=\(queues[q].num))")
                }
            }
        case VIRTIO_MMIO_QUEUE_NOTIFY:
            log("GPU: Queue notify \(value), queue ready=\(queues[Int(value)].ready)")
            processQueue(Int(value))
        case VIRTIO_MMIO_INTERRUPT_ACK:
            interruptStatus &= ~value
        case VIRTIO_MMIO_STATUS:
            status = value
            if value == 0 {
                log("GPU: Device reset")
                resources.removeAll()
                for i in 0..<queues.count {
                    queues[i] = VirtqueueState()
                }
            }
        case VIRTIO_MMIO_QUEUE_DESC_LOW:
            let q = Int(queueSel)
            if q < queues.count {
                queues[q].descAddr = (queues[q].descAddr & 0xFFFFFFFF00000000) | UInt64(value)
            }
        case VIRTIO_MMIO_QUEUE_DESC_HIGH:
            let q = Int(queueSel)
            if q < queues.count {
                queues[q].descAddr = (queues[q].descAddr & 0x00000000FFFFFFFF) | (UInt64(value) << 32)
            }
        case VIRTIO_MMIO_QUEUE_AVAIL_LOW:
            let q = Int(queueSel)
            if q < queues.count {
                queues[q].availAddr = (queues[q].availAddr & 0xFFFFFFFF00000000) | UInt64(value)
            }
        case VIRTIO_MMIO_QUEUE_AVAIL_HIGH:
            let q = Int(queueSel)
            if q < queues.count {
                queues[q].availAddr = (queues[q].availAddr & 0x00000000FFFFFFFF) | (UInt64(value) << 32)
            }
        case VIRTIO_MMIO_QUEUE_USED_LOW:
            let q = Int(queueSel)
            if q < queues.count {
                queues[q].usedAddr = (queues[q].usedAddr & 0xFFFFFFFF00000000) | UInt64(value)
            }
        case VIRTIO_MMIO_QUEUE_USED_HIGH:
            let q = Int(queueSel)
            if q < queues.count {
                queues[q].usedAddr = (queues[q].usedAddr & 0x00000000FFFFFFFF) | (UInt64(value) << 32)
            }
        default:
            break
        }
    }

    func processQueue(_ queueIdx: Int) {
        log("GPU: processQueue(\(queueIdx))")
        guard queueIdx < queues.count && queues[queueIdx].ready else {
            log("GPU: Queue not ready or invalid")
            return
        }

        let queue = queues[queueIdx]
        log("GPU: Queue desc=0x\(String(queue.descAddr, radix: 16)) avail=0x\(String(queue.availAddr, radix: 16)) used=0x\(String(queue.usedAddr, radix: 16))")
        guard let descBase = guestToHost(queue.descAddr),
              let availBase = guestToHost(queue.availAddr),
              let usedBase = guestToHost(queue.usedAddr) else {
            log("GPU: Invalid queue addresses - cannot convert to host")
            return
        }
        log("GPU: Queue addresses valid")

        let availIdx = availBase.advanced(by: 2).loadUnaligned(as: UInt16.self)
        var lastIdx = queues[queueIdx].lastAvailIdx
        log("GPU: availIdx=\(availIdx) lastIdx=\(lastIdx)")

        while lastIdx != availIdx {
            let ringIdx = Int(lastIdx % UInt16(queue.num))
            let descIdx = availBase.advanced(by: 4 + ringIdx * 2).loadUnaligned(as: UInt16.self)

            processDescriptorChain(descBase: descBase, descIdx: Int(descIdx), queueNum: Int(queue.num), usedBase: usedBase, lastIdx: lastIdx, queueNum32: queue.num)

            lastIdx = lastIdx &+ 1
        }

        queues[queueIdx].lastAvailIdx = lastIdx
    }

    func processDescriptorChain(descBase: UnsafeMutableRawPointer, descIdx: Int, queueNum: Int, usedBase: UnsafeMutableRawPointer, lastIdx: UInt16, queueNum32: UInt32) {
        var currentIdx = descIdx
        var cmdAddr: UInt64 = 0
        var cmdLen: UInt32 = 0
        var respAddr: UInt64 = 0
        var respLen: UInt32 = 0

        var iterations = 0
        while iterations < queueNum {
            let descPtr = descBase.advanced(by: currentIdx * 16)
            let addr = descPtr.loadUnaligned(as: UInt64.self)
            let len = descPtr.advanced(by: 8).loadUnaligned(as: UInt32.self)
            let flags = descPtr.advanced(by: 12).loadUnaligned(as: UInt16.self)
            let next = descPtr.advanced(by: 14).loadUnaligned(as: UInt16.self)

            if (flags & VRING_DESC_F_WRITE) != 0 {
                respAddr = addr
                respLen = len
            } else {
                cmdAddr = addr
                cmdLen = len
            }

            if (flags & VRING_DESC_F_NEXT) == 0 {
                break
            }
            currentIdx = Int(next)
            iterations += 1
        }

        if cmdAddr != 0 && respAddr != 0 {
            let respWritten = handleGpuCommand(cmdAddr: cmdAddr, cmdLen: cmdLen, respAddr: respAddr, respLen: respLen)

            let usedIdx = usedBase.advanced(by: 2).loadUnaligned(as: UInt16.self)
            let usedRingIdx = Int(usedIdx % UInt16(queueNum32))
            let usedElemPtr = usedBase.advanced(by: 4 + usedRingIdx * 8)
            usedElemPtr.storeBytes(of: UInt32(descIdx), as: UInt32.self)
            usedElemPtr.advanced(by: 4).storeBytes(of: respWritten, as: UInt32.self)

            usedBase.advanced(by: 2).storeBytes(of: usedIdx &+ 1, as: UInt16.self)
        }
    }

    func handleGpuCommand(cmdAddr: UInt64, cmdLen: UInt32, respAddr: UInt64, respLen: UInt32) -> UInt32 {
        guard let cmdPtr = guestToHost(cmdAddr),
              let respPtr = guestToHost(respAddr) else {
            return 0
        }

        let cmdType = cmdPtr.loadUnaligned(as: UInt32.self)

        switch cmdType {
        case VIRTIO_GPU_CMD_GET_DISPLAY_INFO:
            return handleGetDisplayInfo(respPtr: respPtr, respLen: respLen)
        case VIRTIO_GPU_CMD_RESOURCE_CREATE_2D:
            return handleResourceCreate2D(cmdPtr: cmdPtr, respPtr: respPtr)
        case VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING:
            return handleResourceAttachBacking(cmdPtr: cmdPtr, cmdLen: cmdLen, respPtr: respPtr)
        case VIRTIO_GPU_CMD_SET_SCANOUT:
            return handleSetScanout(cmdPtr: cmdPtr, respPtr: respPtr)
        case VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D:
            return handleTransferToHost2D(cmdPtr: cmdPtr, respPtr: respPtr)
        case VIRTIO_GPU_CMD_RESOURCE_FLUSH:
            return handleResourceFlush(cmdPtr: cmdPtr, respPtr: respPtr)
        default:
            log("GPU: Unknown command 0x\(String(cmdType, radix: 16))")
            respPtr.storeBytes(of: UInt32(0x1200), as: UInt32.self)
            return 24
        }
    }

    func handleGetDisplayInfo(respPtr: UnsafeMutableRawPointer, respLen: UInt32) -> UInt32 {
        log("GPU: GET_DISPLAY_INFO")

        respPtr.storeBytes(of: VIRTIO_GPU_RESP_OK_DISPLAY_INFO, as: UInt32.self)
        respPtr.advanced(by: 4).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 8).storeBytes(of: UInt64(0), as: UInt64.self)
        respPtr.advanced(by: 16).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 20).storeBytes(of: UInt32(0), as: UInt32.self)

        let displayPtr = respPtr.advanced(by: 24)
        displayPtr.storeBytes(of: UInt32(0), as: UInt32.self)
        displayPtr.advanced(by: 4).storeBytes(of: UInt32(0), as: UInt32.self)
        displayPtr.advanced(by: 8).storeBytes(of: scanoutWidth, as: UInt32.self)
        displayPtr.advanced(by: 12).storeBytes(of: scanoutHeight, as: UInt32.self)
        displayPtr.advanced(by: 16).storeBytes(of: UInt32(1), as: UInt32.self)
        displayPtr.advanced(by: 20).storeBytes(of: UInt32(0), as: UInt32.self)

        for i in 1..<16 {
            let ptr = respPtr.advanced(by: 24 + i * 24)
            for j in 0..<24 {
                ptr.advanced(by: j).storeBytes(of: UInt8(0), as: UInt8.self)
            }
        }

        return 24 + 16 * 24
    }

    func handleResourceCreate2D(cmdPtr: UnsafeMutableRawPointer, respPtr: UnsafeMutableRawPointer) -> UInt32 {
        let resourceId = cmdPtr.advanced(by: 24).loadUnaligned(as: UInt32.self)
        let format = cmdPtr.advanced(by: 28).loadUnaligned(as: UInt32.self)
        let width = cmdPtr.advanced(by: 32).loadUnaligned(as: UInt32.self)
        let height = cmdPtr.advanced(by: 36).loadUnaligned(as: UInt32.self)

        log("GPU: RESOURCE_CREATE_2D id=\(resourceId) \(width)x\(height) format=\(format)")

        var resource = GpuResource()
        resource.id = resourceId
        resource.format = format
        resource.width = width
        resource.height = height
        resources[resourceId] = resource

        if width > 0 && height > 0 && width <= 4096 && height <= 4096 {
            framebufferWidth = width
            framebufferHeight = height
            framebuffer = [UInt8](repeating: 0, count: Int(width * height * 4))
        }

        respPtr.storeBytes(of: VIRTIO_GPU_RESP_OK_NODATA, as: UInt32.self)
        respPtr.advanced(by: 4).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 8).storeBytes(of: UInt64(0), as: UInt64.self)
        respPtr.advanced(by: 16).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 20).storeBytes(of: UInt32(0), as: UInt32.self)

        return 24
    }

    func handleResourceAttachBacking(cmdPtr: UnsafeMutableRawPointer, cmdLen: UInt32, respPtr: UnsafeMutableRawPointer) -> UInt32 {
        let resourceId = cmdPtr.advanced(by: 24).loadUnaligned(as: UInt32.self)
        let nrEntries = cmdPtr.advanced(by: 28).loadUnaligned(as: UInt32.self)

        log("GPU: RESOURCE_ATTACH_BACKING id=\(resourceId) entries=\(nrEntries)")

        if var resource = resources[resourceId], nrEntries > 0 {
            let entryAddr = cmdPtr.advanced(by: 32).loadUnaligned(as: UInt64.self)
            let entryLen = cmdPtr.advanced(by: 40).loadUnaligned(as: UInt32.self)

            resource.backingAddr = entryAddr
            resource.backingLength = entryLen
            resources[resourceId] = resource

            log("GPU: Backing at 0x\(String(entryAddr, radix: 16)) len=\(entryLen)")
        }

        respPtr.storeBytes(of: VIRTIO_GPU_RESP_OK_NODATA, as: UInt32.self)
        respPtr.advanced(by: 4).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 8).storeBytes(of: UInt64(0), as: UInt64.self)
        respPtr.advanced(by: 16).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 20).storeBytes(of: UInt32(0), as: UInt32.self)

        return 24
    }

    func handleSetScanout(cmdPtr: UnsafeMutableRawPointer, respPtr: UnsafeMutableRawPointer) -> UInt32 {
        let x = cmdPtr.advanced(by: 24).loadUnaligned(as: UInt32.self)
        let y = cmdPtr.advanced(by: 28).loadUnaligned(as: UInt32.self)
        let w = cmdPtr.advanced(by: 32).loadUnaligned(as: UInt32.self)
        let h = cmdPtr.advanced(by: 36).loadUnaligned(as: UInt32.self)
        let scanoutId = cmdPtr.advanced(by: 40).loadUnaligned(as: UInt32.self)
        let resourceId = cmdPtr.advanced(by: 44).loadUnaligned(as: UInt32.self)

        log("GPU: SET_SCANOUT scanout=\(scanoutId) resource=\(resourceId) rect=(\(x),\(y),\(w),\(h))")

        scanoutResourceId = resourceId

        respPtr.storeBytes(of: VIRTIO_GPU_RESP_OK_NODATA, as: UInt32.self)
        respPtr.advanced(by: 4).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 8).storeBytes(of: UInt64(0), as: UInt64.self)
        respPtr.advanced(by: 16).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 20).storeBytes(of: UInt32(0), as: UInt32.self)

        return 24
    }

    func handleTransferToHost2D(cmdPtr: UnsafeMutableRawPointer, respPtr: UnsafeMutableRawPointer) -> UInt32 {
        let x = cmdPtr.advanced(by: 24).loadUnaligned(as: UInt32.self)
        let y = cmdPtr.advanced(by: 28).loadUnaligned(as: UInt32.self)
        let w = cmdPtr.advanced(by: 32).loadUnaligned(as: UInt32.self)
        let h = cmdPtr.advanced(by: 36).loadUnaligned(as: UInt32.self)
        let offset = cmdPtr.advanced(by: 40).loadUnaligned(as: UInt64.self)
        let resourceId = cmdPtr.advanced(by: 48).loadUnaligned(as: UInt32.self)

        log("GPU: TRANSFER_TO_HOST_2D resource=\(resourceId) rect=(\(x),\(y),\(w),\(h)) offset=\(offset)")

        if let resource = resources[resourceId],
           resource.backingAddr != 0,
           let srcPtr = guestToHost(resource.backingAddr) {

            let srcWidth = resource.width

            for row in 0..<h {
                let srcY = y + row
                if srcY >= resource.height { continue }

                for col in 0..<w {
                    let srcX = x + col
                    if srcX >= srcWidth { continue }

                    let srcOffset = Int((srcY * srcWidth + srcX) * 4)
                    let dstOffset = Int((srcY * framebufferWidth + srcX) * 4)

                    if dstOffset + 4 <= framebuffer.count {
                        framebuffer[dstOffset] = srcPtr.advanced(by: srcOffset).load(as: UInt8.self)
                        framebuffer[dstOffset + 1] = srcPtr.advanced(by: srcOffset + 1).load(as: UInt8.self)
                        framebuffer[dstOffset + 2] = srcPtr.advanced(by: srcOffset + 2).load(as: UInt8.self)
                        framebuffer[dstOffset + 3] = srcPtr.advanced(by: srcOffset + 3).load(as: UInt8.self)
                    }
                }
            }
        }

        respPtr.storeBytes(of: VIRTIO_GPU_RESP_OK_NODATA, as: UInt32.self)
        respPtr.advanced(by: 4).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 8).storeBytes(of: UInt64(0), as: UInt64.self)
        respPtr.advanced(by: 16).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 20).storeBytes(of: UInt32(0), as: UInt32.self)

        return 24
    }

    func handleResourceFlush(cmdPtr: UnsafeMutableRawPointer, respPtr: UnsafeMutableRawPointer) -> UInt32 {
        let x = cmdPtr.advanced(by: 24).loadUnaligned(as: UInt32.self)
        let y = cmdPtr.advanced(by: 28).loadUnaligned(as: UInt32.self)
        let w = cmdPtr.advanced(by: 32).loadUnaligned(as: UInt32.self)
        let h = cmdPtr.advanced(by: 36).loadUnaligned(as: UInt32.self)
        let resourceId = cmdPtr.advanced(by: 40).loadUnaligned(as: UInt32.self)

        log("GPU: RESOURCE_FLUSH resource=\(resourceId) rect=(\(x),\(y),\(w),\(h))")

        flushCount += 1
        saveFramebufferToPPM()

        respPtr.storeBytes(of: VIRTIO_GPU_RESP_OK_NODATA, as: UInt32.self)
        respPtr.advanced(by: 4).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 8).storeBytes(of: UInt64(0), as: UInt64.self)
        respPtr.advanced(by: 16).storeBytes(of: UInt32(0), as: UInt32.self)
        respPtr.advanced(by: 20).storeBytes(of: UInt32(0), as: UInt32.self)

        return 24
    }

    func saveFramebufferToPPM() {
        guard framebufferWidth > 0 && framebufferHeight > 0 && !framebuffer.isEmpty else {
            log("GPU: No framebuffer to save")
            return
        }

        let filename = "/Users/kevin/Desktop/uni/vmm/framebuffer_\(flushCount).ppm"

        var ppmData = Data()
        let header = "P6\n\(framebufferWidth) \(framebufferHeight)\n255\n"
        ppmData.append(contentsOf: header.utf8)

        for y in 0..<Int(framebufferHeight) {
            for x in 0..<Int(framebufferWidth) {
                let offset = (y * Int(framebufferWidth) + x) * 4
                if offset + 2 < framebuffer.count {
                    let b = framebuffer[offset]
                    let g = framebuffer[offset + 1]
                    let r = framebuffer[offset + 2]
                    ppmData.append(r)
                    ppmData.append(g)
                    ppmData.append(b)
                } else {
                    ppmData.append(contentsOf: [0, 0, 0])
                }
            }
        }

        do {
            try ppmData.write(to: URL(fileURLWithPath: filename))
            log("GPU: Saved framebuffer to \(filename) (\(framebufferWidth)x\(framebufferHeight))")
        } catch {
            log("GPU: Failed to save framebuffer: \(error)")
        }
    }
}

// MARK: - Main VMM

log("=== Hypervisor.framework VMM ===")

guard hv_vm_create(nil) == HV_SUCCESS else {
    log("ERROR: Failed to create VM. Hypervisor not available or entitlement missing.")
    exit(1)
}
log("VM created successfully")

var ramPtr: UnsafeMutableRawPointer?
let ramAlloc = posix_memalign(&ramPtr, 0x4000, Int(RAM_SIZE))
guard ramAlloc == 0, let ram = ramPtr else {
    log("ERROR: Failed to allocate RAM")
    exit(1)
}
memset(ram, 0, Int(RAM_SIZE))
log("Allocated \(RAM_SIZE / 1024 / 1024) MB RAM")

guard hv_vm_map(ram, RAM_BASE, Int(RAM_SIZE), UInt64(HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC)) == HV_SUCCESS else {
    log("ERROR: Failed to map RAM")
    exit(1)
}
log("Mapped RAM at 0x\(String(RAM_BASE, radix: 16)) - 0x\(String(RAM_BASE + RAM_SIZE, radix: 16))")

let kernelPath = "/Users/kevin/Desktop/uni/my_unikernel/target/aarch64-unknown-none/release/kernel.bin"
log("Loading kernel from: \(kernelPath)")

guard let kernelData = FileManager.default.contents(atPath: kernelPath) else {
    log("ERROR: Failed to read kernel file")
    exit(1)
}
log("Kernel size: \(kernelData.count) bytes")

_ = kernelData.withUnsafeBytes { ptr in
    memcpy(ram, ptr.baseAddress!, kernelData.count)
}
log("Kernel loaded at 0x\(String(RAM_BASE, radix: 16))")

// Create virtio-GPU device
let gpuDevice = VirtioGpuDevice(ram: ram, ramBase: RAM_BASE)
log("Virtio-GPU device at 0x\(String(VIRTIO_GPU_BASE, radix: 16))")

var vcpu: hv_vcpu_t = 0
var vcpuExit: UnsafeMutablePointer<hv_vcpu_exit_t>?
guard hv_vcpu_create(&vcpu, &vcpuExit, nil) == HV_SUCCESS else {
    log("ERROR: Failed to create vCPU")
    exit(1)
}
log("vCPU created")

guard hv_vcpu_set_reg(vcpu, HV_REG_PC, RAM_BASE) == HV_SUCCESS else {
    log("ERROR: Failed to set PC")
    exit(1)
}

guard hv_vcpu_set_reg(vcpu, HV_REG_CPSR, 0x3c5) == HV_SUCCESS else {
    log("ERROR: Failed to set CPSR")
    exit(1)
}

guard hv_vcpu_set_reg(vcpu, HV_REG_X0, 0) == HV_SUCCESS else {
    log("ERROR: Failed to set X0")
    exit(1)
}

log("vCPU registers configured:")
log("  PC = 0x\(String(RAM_BASE, radix: 16))")
log("  X0 = 0 (no DTB)")
log("  CPSR = 0x3c5 (EL1h, interrupts masked)")

log("")
log("Starting VM execution...")
log("(UART at 0x\(String(PL011_BASE, radix: 16)), GPU at 0x\(String(VIRTIO_GPU_BASE, radix: 16)))")
log("-----------------------------------")

var running = true
var exitCount: UInt64 = 0
let maxExits: UInt64 = 1000000

while running && exitCount < maxExits {
    let ret = hv_vcpu_run(vcpu)

    if ret != HV_SUCCESS {
        log("\nERROR: hv_vcpu_run failed with code \(ret)")
        break
    }

    guard let exit = vcpuExit?.pointee else {
        log("\nERROR: No exit info")
        break
    }

    exitCount += 1

    switch exit.reason {
    case HV_EXIT_REASON_EXCEPTION:
        let syndrome = exit.exception.syndrome
        let ec = (syndrome >> 26) & 0x3F

        var pc: UInt64 = 0
        hv_vcpu_get_reg(vcpu, HV_REG_PC, &pc)

        if ec == 0x24 || ec == 0x25 {
            let far = exit.exception.virtual_address
            let isWrite = (syndrome & (1 << 6)) != 0
            let srt = Int((syndrome >> 16) & 0x1F)

            // UART access
            if far >= PL011_BASE && far < PL011_BASE + PL011_SIZE {
                let offset = far - PL011_BASE

                if isWrite {
                    var value: UInt64 = 0
                    let reg = hv_reg_t(rawValue: UInt32(HV_REG_X0.rawValue) + UInt32(srt))
                    hv_vcpu_get_reg(vcpu, reg, &value)

                    if offset == 0 {
                        let char = UInt8(value & 0xFF)
                        var c = char
                        write(STDOUT_FILENO, &c, 1)
                    }
                } else {
                    let reg = hv_reg_t(rawValue: UInt32(HV_REG_X0.rawValue) + UInt32(srt))
                    hv_vcpu_set_reg(vcpu, reg, 0)
                }

                hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4)
                continue
            }

            // GPU access
            if far >= VIRTIO_GPU_BASE && far < VIRTIO_GPU_BASE + VIRTIO_GPU_SIZE {
                let offset = far - VIRTIO_GPU_BASE

                // Check if ISV (Instruction Syndrome Valid) bit is set
                let isv = (syndrome & (1 << 24)) != 0

                if isWrite {
                    var value: UInt64 = 0

                    // Always decode instruction manually - ISV seems unreliable for device MMIO
                    if pc >= RAM_BASE && pc < RAM_BASE + RAM_SIZE {
                        let instrOffset = Int(pc - RAM_BASE)
                        let instr = ram.advanced(by: instrOffset).loadUnaligned(as: UInt32.self)

                        // Extract Rt (bits 4:0) - source register for store
                        let rt = Int(instr & 0x1F)

                        // Rt=31 is XZR/WZR (zero register), value is 0
                        if rt == 31 {
                            value = 0
                        } else {
                            let reg = hv_reg_t(rawValue: UInt32(HV_REG_X0.rawValue) + UInt32(rt))
                            hv_vcpu_get_reg(vcpu, reg, &value)
                        }
                    }

                    gpuDevice.write32(offset, UInt32(value & 0xFFFFFFFF))
                } else {
                    let value = gpuDevice.read32(offset)

                    if isv {
                        let reg = hv_reg_t(rawValue: UInt32(HV_REG_X0.rawValue) + UInt32(srt))
                        hv_vcpu_set_reg(vcpu, reg, UInt64(value))
                    } else {
                        // Decode LDR instruction to find target register
                        if pc >= RAM_BASE && pc < RAM_BASE + RAM_SIZE {
                            let instrOffset = Int(pc - RAM_BASE)
                            let instr = ram.advanced(by: instrOffset).loadUnaligned(as: UInt32.self)
                            let rt = Int(instr & 0x1F)
                            let reg = hv_reg_t(rawValue: UInt32(HV_REG_X0.rawValue) + UInt32(rt))
                            hv_vcpu_set_reg(vcpu, reg, UInt64(value))
                        }
                    }
                }

                hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4)
                continue
            }

            // Unknown MMIO read - return 0
            if !isWrite {
                let reg = hv_reg_t(rawValue: UInt32(HV_REG_X0.rawValue) + UInt32(srt))
                hv_vcpu_set_reg(vcpu, reg, 0)
                hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4)
                continue
            }

            // Unknown MMIO write - ignore and continue
            hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4)
            continue

        } else if ec == 0x16 {
            log("\nHVC at PC=0x\(String(pc, radix: 16))")
            hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4)
            continue

        } else if ec == 0x01 {
            log("\n-----------------------------------")
            log("VM halted (WFI at PC=0x\(String(pc, radix: 16)))")
            running = false

        } else if ec == 0x00 {
            log("\nUnknown exception at PC=0x\(String(pc, radix: 16))")
            log("  Syndrome=0x\(String(syndrome, radix: 16))")
            running = false

        } else {
            log("\nException at PC=0x\(String(pc, radix: 16))")
            log("  EC=0x\(String(ec, radix: 16))")
            log("  Syndrome=0x\(String(syndrome, radix: 16))")
            running = false
        }

    case HV_EXIT_REASON_CANCELED:
        log("\nVM canceled")
        running = false

    case HV_EXIT_REASON_VTIMER_ACTIVATED:
        continue

    default:
        log("\nUnhandled exit reason: \(exit.reason.rawValue)")
        running = false
    }
}

log("-----------------------------------")
if exitCount >= maxExits {
    log("Stopped after reaching exit limit (\(maxExits))")
} else {
    log("VM stopped after \(exitCount) exits")
}

var finalPC: UInt64 = 0
hv_vcpu_get_reg(vcpu, HV_REG_PC, &finalPC)
log("Final PC=0x\(String(finalPC, radix: 16))")

let debugOffset = 0x000F_0000
let debugPtr = ram.advanced(by: debugOffset)
let magic = debugPtr.loadUnaligned(as: UInt32.self)
log("\n=== Debug Area ===")
log("Magic: 0x\(String(magic, radix: 16))")
if magic == 0xDEAD_BEEF {
    let modePtr = debugPtr.advanced(by: 4)
    var modeStr = ""
    for i in 0..<10 {
        let c = modePtr.advanced(by: i).load(as: UInt8.self)
        if c == 0 { break }
        modeStr.append(Character(UnicodeScalar(c)))
    }
    log("Output mode: \(modeStr)")

    let complete = debugPtr.advanced(by: 252).loadUnaligned(as: UInt32.self)
    log("Complete marker: 0x\(String(complete, radix: 16))")
}

log("\n=== GPU Summary ===")
log("Resources created: \(gpuDevice.resources.count)")
log("Flush count: \(gpuDevice.flushCount)")
if gpuDevice.framebufferWidth > 0 && gpuDevice.framebufferHeight > 0 {
    log("Framebuffer: \(gpuDevice.framebufferWidth)x\(gpuDevice.framebufferHeight)")
}

hv_vcpu_destroy(vcpu)
hv_vm_destroy()
free(ram)
