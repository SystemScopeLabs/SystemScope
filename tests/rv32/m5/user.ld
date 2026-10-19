/* user.ld: links the M5 programs that have only code and constants (counter, handoff)
 * at USER_BASE (docs/m5-design.md §12.2). The constants share the text segment, which is
 * readable. user-bss.ld is the same with a bss segment, for reader and spinner.
 */
OUTPUT_ARCH("riscv")
ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);   /* R X */
}

SECTIONS
{
    . = 0x10000;
    .text : { *(.text.init) *(.text .text.*) } :text
    .rodata : { *(.rodata .rodata.*) } :text
    .data : { *(.data .data.*) *(.bss .bss.*) } :text
    ASSERT(SIZEOF(.data) == 0, "a program with data or bss must be linked with user-bss.ld")
    /DISCARD/ : { *(.comment) *(.note .note.*) }
}
