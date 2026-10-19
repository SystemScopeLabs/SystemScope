/* user-bss.ld: links the M5 programs with a buffer (reader, spinner) at USER_BASE
 * (docs/m5-design.md §12.2): text with the constants in its segment, then bss on a fresh
 * page, because the kernel maps permissions per page (docs/m3-design.md §8.3).
 */
OUTPUT_ARCH("riscv")
ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);   /* R X */
    bss PT_LOAD FLAGS(6);    /* R W */
}

SECTIONS
{
    . = 0x10000;
    .text : { *(.text.init) *(.text .text.*) } :text
    .rodata : { *(.rodata .rodata.*) } :text
    .data : { *(.data .data.*) } :text
    ASSERT(SIZEOF(.data) == 0, "an M5 program keeps no initialized data")
    . = ALIGN(0x1000);
    .bss : { *(.bss .bss.*) } :bss
    /DISCARD/ : { *(.comment) *(.note .note.*) }
}
