// Headless test VMM for automated testing
// Runs kernel, captures output, checks for expected strings, exits with status code

import Virtualization
import Foundation

// Disable buffering
setbuf(stdout, nil)
setbuf(stderr, nil)

// Test configuration
let TIMEOUT_SECONDS: Double = 30
let EXPECTED_STRINGS = [
    "DTB:",
    "Looking for GPU",
    "Found virtio-gpu",
    "Display initialized",
    "Graphics rendered",
    "Halting"
]

// Track test state
var capturedOutput = ""
var foundStrings = Set<String>()
var testPassed = false
var vmStarted = false

// Paths
let kernelPath = ProcessInfo.processInfo.environment["KERNEL_PATH"]
    ?? "/Users/kevin/Desktop/uni/my_unikernel/target/aarch64-unknown-none/release/Image"

print("=== Unikernel Automated Test ===")
print("Kernel: \(kernelPath)")
print("Timeout: \(Int(TIMEOUT_SECONDS))s")
print("Expected strings: \(EXPECTED_STRINGS.count)")
print("")

// Check kernel exists
if !FileManager.default.fileExists(atPath: kernelPath) {
    print("ERROR: Kernel not found at \(kernelPath)")
    exit(2)
}

// Configuration
let config = VZVirtualMachineConfiguration()
config.cpuCount = 1
config.memorySize = 1024 * 1024 * 1024
config.platform = VZGenericPlatformConfiguration()

// Boot loader
let bootLoader = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: kernelPath))
bootLoader.commandLine = "console=hvc0"
config.bootLoader = bootLoader

// Serial port - capture output
let inputPipe = Pipe()
let outputPipe = Pipe()

outputPipe.fileHandleForReading.readabilityHandler = { handle in
    let data = handle.availableData
    if data.count > 0 {
        if let str = String(data: data, encoding: .utf8) {
            capturedOutput += str

            // Check for expected strings
            for expected in EXPECTED_STRINGS {
                if str.contains(expected) || capturedOutput.contains(expected) {
                    if !foundStrings.contains(expected) {
                        foundStrings.insert(expected)
                        print("✓ Found: \"\(expected)\"")
                    }
                }
            }

            // Check if all strings found
            if foundStrings.count == EXPECTED_STRINGS.count && !testPassed {
                testPassed = true
                print("")
                print("=== ALL EXPECTED STRINGS FOUND ===")
            }

            // Print serial output with prefix
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

// Graphics device (headless but needed for GPU driver to work)
let graphics = VZVirtioGraphicsDeviceConfiguration()
graphics.scanouts = [VZVirtioGraphicsScanoutConfiguration(widthInPixels: 1280, heightInPixels: 720)]
config.graphicsDevices = [graphics]

// Network (needed for full PCI bus)
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
    print("ERROR: Config validation failed: \(error)")
    exit(2)
}

let vm = VZVirtualMachine(configuration: config)

class VMDelegate: NSObject, VZVirtualMachineDelegate {
    func virtualMachine(_ vm: VZVirtualMachine, didStopWithError error: Error) {
        print("\nVM stopped with error: \(error)")
        printResults()
        exit(testPassed ? 0 : 1)
    }
    func guestDidStop(_ vm: VZVirtualMachine) {
        print("\nVM stopped cleanly")
        printResults()
        exit(testPassed ? 0 : 1)
    }
}

func printResults() {
    print("")
    print("=== TEST RESULTS ===")
    print("Found \(foundStrings.count)/\(EXPECTED_STRINGS.count) expected strings")

    let missing = Set(EXPECTED_STRINGS).subtracting(foundStrings)
    if !missing.isEmpty {
        print("Missing:")
        for s in missing {
            print("  ✗ \"\(s)\"")
        }
    }

    print("")
    if testPassed {
        print("TEST PASSED ✓")
    } else {
        print("TEST FAILED ✗")
    }
}

let vmDelegate = VMDelegate()
vm.delegate = vmDelegate

// Timeout timer
let timeoutTimer = DispatchSource.makeTimerSource(queue: .main)
timeoutTimer.schedule(deadline: .now() + TIMEOUT_SECONDS)
timeoutTimer.setEventHandler {
    print("\n=== TIMEOUT after \(Int(TIMEOUT_SECONDS))s ===")

    // Try to stop VM gracefully
    if vm.canRequestStop {
        do {
            try vm.requestStop()
        } catch {
            print("Stop error: \(error)")
        }
    }

    // Give it a moment, then exit
    DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
        printResults()
        exit(testPassed ? 0 : 1)
    }
}
timeoutTimer.resume()

// Success timer - exit shortly after all strings found
let checkTimer = DispatchSource.makeTimerSource(queue: .main)
checkTimer.schedule(deadline: .now() + 1, repeating: 0.5)
checkTimer.setEventHandler {
    if testPassed && vmStarted {
        // Wait a bit more to capture "Halting" message
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) {
            print("\n=== Test complete, stopping VM ===")
            if vm.canRequestStop {
                try? vm.requestStop()
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                printResults()
                exit(0)
            }
        }
        checkTimer.cancel()
    }
}
checkTimer.resume()

// Start VM
print("Starting VM...")
vm.start { result in
    switch result {
    case .success:
        vmStarted = true
        print("VM running")
        print("")
    case .failure(let error):
        print("Start failed: \(error)")
        exit(2)
    }
}

// Run the run loop
RunLoop.main.run()
