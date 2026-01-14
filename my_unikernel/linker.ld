/* Linker script for aarch64 unikernel on macOS Hypervisor.framework */

ENTRY(_start)

/* Must match RAM_BASE in the VMM
 * 0x70000000 for VZ compatibility (0x40000000 is PCI ECAM in VZ)
 */
RAM_BASE = 0x70000000;

SECTIONS
{
    . = RAM_BASE;

    /* Boot code must come first */
    .text.boot : {
        *(.text.boot)
    }

    .text : {
        *(.text .text.*)
    }

    . = ALIGN(4096);

    .rodata : {
        *(.rodata .rodata.*)
    }

    . = ALIGN(4096);

    .data : {
        *(.data .data.*)
    }

    . = ALIGN(4096);

    _bss_start = .;
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }
    _bss_end = .;

    . = ALIGN(4096);

    _end = .;

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note*)
    }
}
