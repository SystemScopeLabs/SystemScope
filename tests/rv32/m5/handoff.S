# handoff: entry 5 of the M5 reference disk (docs/m5-design.md §12.2). Sends the values
# 1 to COUNT to spinner through a one-slot mailbox on the shared page: it waits until the
# flag is clear, writes the data, then sets the flag. spinner reads the data, then clears
# the flag. While the mailbox is full it yields. It prints nothing and exits 0; the
# mailbox is dumped at shutdown (data COUNT, flag 0).
#
# gp-independent (m3-design §17.1): linked without relaxation.

    .option norelax

    .equ SYS_EXIT, 93
    .equ SYS_SCHED_YIELD, 124
    .equ COUNT, 8

    .equ SHARED_VA, 0xF000
    .equ DATA, 0x4
    .equ FULL, 0x8

    .section .text.init, "ax"
    .globl _start
_start:
    li s0, SHARED_VA
    li s1, 1
    li s2, COUNT
    li s3, 1
1:  lw t0, FULL(s0)
    beqz t0, 2f
    li a7, SYS_SCHED_YIELD
    ecall
    j 1b
2:  sw s1, DATA(s0)
    sw s3, FULL(s0)
    addi s1, s1, 1
    bge s2, s1, 1b
    li a0, 0
    li a7, SYS_EXIT
    ecall
