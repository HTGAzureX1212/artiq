INCLUDE generated/output_format.ld
INCLUDE generated/regions.ld
ENTRY(_reset_handler)

/* First 64M of main memory are reserved for runtime
 * code/data/heap, then comes kernel memory.
 * Next 16M of main memory are reserved for
 * the background RPC queue.
 * First 384K of kernel memory are for support code.
 * Support code is loaded at ORIGIN-0x80 so that ELF headers
 * are also loaded.
 * The next 128M of kernel memory is the kernel heap.
 */
MEMORY {
    ksupport (RWX) : ORIGIN = 0x45000000, LENGTH = 0x8060000
}

/* Kernel stack is at the end of main RAM. */
_fstack = ORIGIN(main_ram) + LENGTH(main_ram) - 16;

/* Force ld to make the ELF header as loadable. */
PHDRS
{
    text     PT_LOAD FILEHDR PHDRS;
    eh_frame PT_GNU_EH_FRAME;
}

SECTIONS
{
    .vectors : {
        *(.vectors)
    } :text

    .text :
    {
        *(.text .text.*)
    } :text

    .rodata :
    {
        *(.rodata .rodata.*)
    } > ksupport

    .eh_frame :
    {
        KEEP(*(.eh_frame))
    } > ksupport :text

    .eh_frame_hdr :
    {
        KEEP(*(.eh_frame_hdr))
    } > ksupport :text :eh_frame

    .gcc_except_table :
    {
        *(.gcc_except_table)
    } > ksupport

    .data :
    {
        *(.data .data.*)
    }

    .bss (NOLOAD) : ALIGN(4)
    {
        _fbss = .;
        *(.sbss .sbss.* .bss .bss.*);
        . = ALIGN(4);
        _ebss = .;
    }

    .heap (NOLOAD) : ALIGN(8)
    {
        _fheap_1 = .;
        . += 0x8000000;
        _eheap_1 = .;
    }

    /DISCARD/ :
    {
        *(.debug*)
    }
}
