#!/usr/bin/env python3
"""
Create an ARM64 Linux kernel Image from a flat binary.
The Image format has a specific 64-byte header required by the boot protocol.
"""

import sys
import struct
import os

def make_arm64_image(input_bin, output_image, load_offset=0x0):
    """Create ARM64 Linux Image with proper header."""

    with open(input_bin, 'rb') as f:
        kernel_data = f.read()

    kernel_size = len(kernel_data)

    # ARM64 Image header (64 bytes)
    # code0: Branch to kernel start (skip header) - "b #0x40" = 0x14000010
    code0 = 0x14000010
    code1 = 0x00000000
    text_offset = load_offset
    image_size = kernel_size + 64  # Include header size
    flags = 0x0  # Little endian, 4K pages, no relocation needed
    res2 = 0
    res3 = 0
    res4 = 0
    magic = 0x644d5241  # "ARM\x64" in little-endian
    res5 = 0

    # Pack header
    header = struct.pack('<IIQQQQQQI4x',
        code0,
        code1,
        text_offset,
        image_size,
        flags,
        res2,
        res3,
        res4,
        magic)

    assert len(header) == 64, f"Header must be 64 bytes, got {len(header)}"

    with open(output_image, 'wb') as f:
        f.write(header)
        f.write(kernel_data)

    print(f"Created ARM64 Image: {output_image}")
    print(f"  Header size: 64 bytes")
    print(f"  Kernel size: {kernel_size} bytes")
    print(f"  Total size: {os.path.getsize(output_image)} bytes")
    print(f"  Load offset: 0x{text_offset:x}")

if __name__ == '__main__':
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <input.bin> <output.Image>")
        sys.exit(1)

    make_arm64_image(sys.argv[1], sys.argv[2])
