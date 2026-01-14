//! DTB (Device Tree Blob) parser for finding PCI MMIO windows
//!
//! This parses the DTB that VZ provides to find the exact MMIO address range
//! that is authorized for PCI BAR allocation. This is how Linux does it.

use core::ptr::read_volatile;

const FDT_BEGIN_NODE: u32 = 0x1;
const FDT_END_NODE: u32 = 0x2;
const FDT_PROP: u32 = 0x3;
const FDT_END: u32 = 0x9;

/// Find the PCI 32-bit MMIO window from the DTB
/// Returns (base_address, size) if found
pub unsafe fn find_pci_mmio_window(dtb_ptr: u64) -> Option<(u64, u64)> {
    if dtb_ptr == 0 {
        return None;
    }

    // Check DTB magic
    let magic = read_volatile(dtb_ptr as *const u32).swap_bytes();
    if magic != 0xd00dfeed {
        return None;
    }

    let off_struct = read_volatile((dtb_ptr + 8) as *const u32).swap_bytes() as u64;
    let off_strings = read_volatile((dtb_ptr + 12) as *const u32).swap_bytes() as u64;
    let total_size = read_volatile((dtb_ptr + 4) as *const u32).swap_bytes() as u64;

    let struct_ptr = dtb_ptr + off_struct;
    let strings_ptr = dtb_ptr + off_strings;
    let max_offset = total_size - off_struct;

    let mut ptr = struct_ptr;
    let mut in_pci_node = false;
    let mut depth = 0u32;
    let mut pci_depth = 0u32;

    while (ptr - struct_ptr) < max_offset {
        let token = read_volatile(ptr as *const u32).swap_bytes();
        ptr += 4;

        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                // Read node name
                let name_start = ptr;
                let mut name_len = 0u64;
                while name_len < 128 {
                    let c = read_volatile((name_start + name_len) as *const u8);
                    if c == 0 {
                        break;
                    }
                    name_len += 1;
                }

                // Check if this is a PCI node (name starts with "pci" or "pcie")
                if name_len >= 3 {
                    let name_slice = core::slice::from_raw_parts(name_start as *const u8, name_len as usize);
                    if let Ok(name) = core::str::from_utf8(name_slice) {
                        if name.starts_with("pci") {
                            in_pci_node = true;
                            pci_depth = depth;
                        }
                    }
                }

                // Align to 4 bytes
                ptr += (name_len + 1 + 3) & !3;
            }
            FDT_END_NODE => {
                if in_pci_node && depth == pci_depth {
                    in_pci_node = false;
                }
                depth = depth.saturating_sub(1);
            }
            FDT_PROP => {
                let len = read_volatile(ptr as *const u32).swap_bytes() as u64;
                let nameoff = read_volatile((ptr + 4) as *const u32).swap_bytes() as u64;
                ptr += 8;

                // Get property name
                let prop_name = get_string(strings_ptr, nameoff);

                // Look for "ranges" property in PCI node
                if in_pci_node && prop_name == "ranges" && len >= 28 {
                    // Parse ranges entries
                    // Format for PCI: <child_hi child_mid child_lo parent_hi parent_lo size_hi size_lo>
                    // Each cell is 4 bytes, 7 cells = 28 bytes per entry
                    let num_entries = len / 28;

                    for i in 0..num_entries {
                        let entry_ptr = ptr + (i * 28);

                        // child_hi encodes the address space type
                        // 0x02000000 = 32-bit MMIO (non-prefetchable)
                        // 0x42000000 = 32-bit MMIO (prefetchable)
                        // 0x03000000 = 64-bit MMIO
                        let child_hi = read_volatile(entry_ptr as *const u32).swap_bytes();

                        // Check for 32-bit MMIO (we want non-prefetchable for devices)
                        if (child_hi & 0x03000000) == 0x02000000 {
                            // Found 32-bit MMIO window
                            let parent_hi = read_volatile((entry_ptr + 12) as *const u32).swap_bytes();
                            let parent_lo = read_volatile((entry_ptr + 16) as *const u32).swap_bytes();
                            let size_hi = read_volatile((entry_ptr + 20) as *const u32).swap_bytes();
                            let size_lo = read_volatile((entry_ptr + 24) as *const u32).swap_bytes();

                            let base = ((parent_hi as u64) << 32) | (parent_lo as u64);
                            let size = ((size_hi as u64) << 32) | (size_lo as u64);

                            if base != 0 && size != 0 {
                                return Some((base, size));
                            }
                        }
                    }
                }

                // Align to 4 bytes
                ptr += (len + 3) & !3;
            }
            FDT_END => break,
            _ => {
                // Unknown token, skip
            }
        }
    }

    None
}

/// Get a null-terminated string from the strings block
unsafe fn get_string(strings_base: u64, offset: u64) -> &'static str {
    let ptr = (strings_base + offset) as *const u8;
    let mut len = 0usize;

    while len < 64 {
        if read_volatile(ptr.add(len)) == 0 {
            break;
        }
        len += 1;
    }

    let slice = core::slice::from_raw_parts(ptr, len);
    core::str::from_utf8_unchecked(slice)
}

/// Find ECAM base address from DTB (for completeness)
pub unsafe fn find_ecam_base(dtb_ptr: u64) -> Option<u64> {
    if dtb_ptr == 0 {
        return None;
    }

    let magic = read_volatile(dtb_ptr as *const u32).swap_bytes();
    if magic != 0xd00dfeed {
        return None;
    }

    let off_struct = read_volatile((dtb_ptr + 8) as *const u32).swap_bytes() as u64;
    let off_strings = read_volatile((dtb_ptr + 12) as *const u32).swap_bytes() as u64;
    let total_size = read_volatile((dtb_ptr + 4) as *const u32).swap_bytes() as u64;

    let struct_ptr = dtb_ptr + off_struct;
    let strings_ptr = dtb_ptr + off_strings;
    let max_offset = total_size - off_struct;

    let mut ptr = struct_ptr;
    let mut in_pci_node = false;

    while (ptr - struct_ptr) < max_offset {
        let token = read_volatile(ptr as *const u32).swap_bytes();
        ptr += 4;

        match token {
            FDT_BEGIN_NODE => {
                let name_start = ptr;
                let mut name_len = 0u64;
                while name_len < 128 {
                    let c = read_volatile((name_start + name_len) as *const u8);
                    if c == 0 {
                        break;
                    }
                    name_len += 1;
                }

                if name_len >= 3 {
                    let name_slice = core::slice::from_raw_parts(name_start as *const u8, name_len as usize);
                    if let Ok(name) = core::str::from_utf8(name_slice) {
                        if name.starts_with("pci") {
                            in_pci_node = true;
                        }
                    }
                }

                ptr += (name_len + 1 + 3) & !3;
            }
            FDT_END_NODE => {
                in_pci_node = false;
            }
            FDT_PROP => {
                let len = read_volatile(ptr as *const u32).swap_bytes() as u64;
                let nameoff = read_volatile((ptr + 4) as *const u32).swap_bytes() as u64;
                ptr += 8;

                let prop_name = get_string(strings_ptr, nameoff);

                // "reg" property in PCI node contains ECAM base
                if in_pci_node && prop_name == "reg" && len >= 16 {
                    let addr_hi = read_volatile(ptr as *const u32).swap_bytes() as u64;
                    let addr_lo = read_volatile((ptr + 4) as *const u32).swap_bytes() as u64;
                    let ecam_base = (addr_hi << 32) | addr_lo;

                    if ecam_base != 0 {
                        return Some(ecam_base);
                    }
                }

                ptr += (len + 3) & !3;
            }
            FDT_END => break,
            _ => {}
        }
    }

    None
}
