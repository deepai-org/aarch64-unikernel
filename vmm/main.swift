import Virtualization
import Foundation

setbuf(stdout, nil)
setbuf(stderr, nil)

// Toggle between Linux and unikernel
let useLinux = true

let kernelPath: String
var initrdPath: String? = nil

if useLinux {
    fputs("=== Booting Debian Linux ===\n", stderr)
    kernelPath = "./linux-debian"
    initrdPath = "./initrd-debian"
} else {
    fputs("=== Booting Unikernel ===\n", stderr)
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
bootLoader.commandLine = "console=hvc0 loglevel=8"
config.bootLoader = bootLoader

// Serial port with pipes
let inputPipe = Pipe()
let outputPipe = Pipe()

// Use DispatchSource for reading - works with dispatchMain()
let readFD = outputPipe.fileHandleForReading.fileDescriptor
let readSource = DispatchSource.makeReadSource(fileDescriptor: readFD, queue: .main)
readSource.setEventHandler {
    let data = outputPipe.fileHandleForReading.availableData
    if data.count > 0 {
        FileHandle.standardOutput.write(data)
    }
}
readSource.resume()

let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
serial.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: inputPipe.fileHandleForReading,
    fileHandleForWriting: outputPipe.fileHandleForWriting
)
config.serialPorts = [serial]

// Add entropy
config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

// Validate
do {
    try config.validate()
    fputs("Config validated\n", stderr)
} catch {
    fputs("ERROR: \(error)\n", stderr)
    exit(1)
}

let vm = VZVirtualMachine(configuration: config)

class Delegate: NSObject, VZVirtualMachineDelegate {
    func virtualMachine(_ vm: VZVirtualMachine, didStopWithError error: Error) {
        fputs("\nVM error: \(error)\n", stderr)
        exit(1)
    }
    func guestDidStop(_ vm: VZVirtualMachine) {
        fputs("\nVM stopped cleanly\n", stderr)
        exit(0)
    }
}

let delegate = Delegate()
vm.delegate = delegate

fputs("Starting VM...\n", stderr)

vm.start { result in
    switch result {
    case .success:
        fputs("VM running\n", stderr)
    case .failure(let error):
        fputs("Start failed: \(error)\n", stderr)
        exit(1)
    }
}

dispatchMain()
