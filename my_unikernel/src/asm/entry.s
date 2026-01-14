// aarch64 entry point for Linux boot protocol
// x0 = DTB pointer (passed by hypervisor)
// x1, x2, x3 = 0

.section .text.boot
.global _start

_start:
    // ================================================================
    // CRITICAL: Enable FP/SIMD FIRST (before ANY Rust code)
    // ================================================================
    // The Rust compiler uses SIMD registers (q0, q1) for memcpy/memset.
    // If FPU is disabled (default), these instructions cause a silent
    // exception trap, causing ~70% of boots to fail.

    // Read CPACR_EL1 (Architectural Feature Access Control Register)
    mrs x1, cpacr_el1

    // Set bits 20-21 (FPEN) to 0b11 to enable EL1 access to FP/SIMD
    orr x1, x1, #(3 << 20)

    // Write back
    msr cpacr_el1, x1

    // Synchronization barrier to ensure the write takes effect
    isb

    // ================================================================
    // Invalidate Instruction Cache
    // ================================================================
    // VZ loads the kernel into RAM (D-Cache), but CPU fetches from I-Cache.
    // These are not coherent on ARM64. Invalidate I-Cache to ensure we
    // execute the code VZ just loaded, not stale garbage.
    ic iallu
    dsb nsh
    isb

    // ================================================================
    // Now proceed with normal boot
    // ================================================================

    // 1. Save DTB pointer (x0) to callee-saved register
    mov x19, x0

    // 2. Disable interrupts
    msr daifset, #0xf

    // 3. Set up stack pointer (16-byte aligned)
    adrp x0, _stack_top
    add x0, x0, :lo12:_stack_top
    mov sp, x0

    // 4. Clear BSS section (using general registers, not SIMD)
    adrp x0, _bss_start
    add x0, x0, :lo12:_bss_start
    adrp x1, _bss_end
    add x1, x1, :lo12:_bss_end
clear_bss:
    cmp x0, x1
    b.ge bss_done
    str xzr, [x0], #8
    b clear_bss
bss_done:

    // 5. Pass DTB pointer as first argument to kmain
    mov x0, x19
    bl kmain

    // 6. If kmain returns, halt
halt:
    wfi
    b halt

// Stack (16KB, 16-byte aligned)
.section .bss
.align 4
_stack_bottom:
    .space 16384
.align 4
_stack_top:
