import Virtualization
import AppKit
import Foundation

setbuf(stdout, nil)
setbuf(stderr, nil)

// Set up app before creating VM
let app = NSApplication.shared
app.setActivationPolicy(.regular)

// Toggle between Linux and unikernel
let useLinux = false  // Back to unikernel

let kernelPath: String
var initrdPath: String? = nil

if useLinux {
    print("=== Booting Debian Linux with GUI ===")
    kernelPath = "./linux-debian"
    initrdPath = "./initrd-debian"
} else {
    print("=== Booting Unikernel with GUI ===")
    kernelPath = "/Users/kevin/Desktop/uni/my_unikernel/target/aarch64-unknown-none/release/Image"
}

// Configuration
let config = VZVirtualMachineConfiguration()
config.cpuCount = 1
config.memorySize = 1024 * 1024 * 1024
config.platform = VZGenericPlatformConfiguration()

// Boot loader
let bootLoader = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: kernelPath))
if let initrd = initrdPath {
    bootLoader.initialRamdiskURL = URL(fileURLWithPath: initrd)
}
// Higher loglevel to see PCI ECAM info
bootLoader.commandLine = "console=tty0 console=hvc0 rdinit=/init loglevel=8 earlyprintk"
config.bootLoader = bootLoader

// Serial port for console output - use pipes (no null device)
let inputPipe = Pipe()
let outputPipe = Pipe()
outputPipe.fileHandleForReading.readabilityHandler = { handle in
    let data = handle.availableData
    if data.count > 0 {
        if let str = String(data: data, encoding: .utf8) {
            print("[SERIAL] \(str)", terminator: "")
        }
    }
}
let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
serial.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: inputPipe.fileHandleForReading,
    fileHandleForWriting: outputPipe.fileHandleForWriting
)
config.serialPorts = [serial]

// Graphics device for display
let graphics = VZVirtioGraphicsDeviceConfiguration()
graphics.scanouts = [VZVirtioGraphicsScanoutConfiguration(widthInPixels: 1280, heightInPixels: 720)]
config.graphicsDevices = [graphics]

// Keyboard and mouse
config.keyboards = [VZUSBKeyboardConfiguration()]
config.pointingDevices = [VZUSBScreenCoordinatePointingDeviceConfiguration()]

// Network
let networkDevice = VZVirtioNetworkDeviceConfiguration()
networkDevice.attachment = VZNATNetworkDeviceAttachment()
config.networkDevices = [networkDevice]

// Entropy
config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

// Validate
do {
    try config.validate()
    print("Config validated")
} catch {
    print("ERROR: \(error)")
    exit(1)
}

let vm = VZVirtualMachine(configuration: config)

class VMDelegate: NSObject, VZVirtualMachineDelegate {
    func virtualMachine(_ vm: VZVirtualMachine, didStopWithError error: Error) {
        print("\nVM error: \(error)")
    }
    func guestDidStop(_ vm: VZVirtualMachine) {
        print("\nVM stopped cleanly")
        NSApp.terminate(nil)
    }
}

let vmDelegate = VMDelegate()
vm.delegate = vmDelegate

// Create window with VM view
class AppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow!
    let vm: VZVirtualMachine

    init(vm: VZVirtualMachine) {
        self.vm = vm
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Create VM view
        let vmView = VZVirtualMachineView()
        vmView.virtualMachine = vm
        vmView.capturesSystemKeys = true

        // Create window
        window = NSWindow(
            contentRect: NSRect(x: 100, y: 100, width: 1280, height: 720),
            styleMask: [.titled, .closable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "VZ Linux VM"
        window.contentView = vmView
        window.makeKeyAndOrderFront(nil)

        // Start VM
        print("Starting VM...")
        vm.start { result in
            switch result {
            case .success:
                print("VM running (state=\(self.vm.state.rawValue))")
            case .failure(let error):
                print("Start failed: \(error)")
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        return true
    }
}

let appDelegate = AppDelegate(vm: vm)
app.delegate = appDelegate
app.activate(ignoringOtherApps: true)
app.run()
