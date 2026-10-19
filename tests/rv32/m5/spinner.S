# spinner: entry 4 of the M5 reference disk (docs/m5-design.md §12.2). Computes STEPS
# xorshift32 steps without a syscall, so only preemption interleaves it, and polls
# handoff's mailbox inside the loop; afterwards it polls, yielding between polls, until
# all COUNT values have arrived. Then it prints "spinner DDDDDDDD XXXXXXXX\n": the
# digest of the values in the order received (d = 31 * d + v) and the final xorshift
# state, both in hex, and exits 0.
# Both are independent of where interrupts land.
#
# gp-independent (m3-design §17.1): linked without relaxation.

    .option norelax

    .equ SYS_WRITE, 64
    .equ SYS_EXIT, 93
    .equ SYS_SCHED_YIELD, 124
    .equ STEPS, 500
    .equ COUNT, 8
    .equ SEED, 0x9E3779B9

    .equ SHARED_VA, 0xF000
    .equ DATA, 0x4
    .equ FULL, 0x8

    .equ PREFIX_LEN, 8              # "spinner "
    .equ LINE_LEN, 26

    .section .text.init, "ax"
    .globl _start
_start:
    li s0, SHARED_VA
    li s1, SEED                     # x
    li s2, STEPS
    li s3, 0                        # digest
    li s4, 0                        # values received
1:  slli t0, s1, 13
    xor s1, s1, t0
    srli t0, s1, 17
    xor s1, s1, t0
    slli t0, s1, 5
    xor s1, s1, t0
    jal ra, poll
    addi s2, s2, -1
    bnez s2, 1b
2:  li t0, COUNT
    beq s4, t0, 3f
    li a7, SYS_SCHED_YIELD
    ecall
    jal ra, poll
    j 2b
3:  # The line: the prefix, the digest, a space, the state, a newline.
    la s5, line
    la t0, prefix
    li t1, PREFIX_LEN
    mv t2, s5
4:  lbu t3, 0(t0)
    sb t3, 0(t2)
    addi t0, t0, 1
    addi t2, t2, 1
    addi t1, t1, -1
    bnez t1, 4b
    mv a0, s3
    addi a1, s5, PREFIX_LEN
    jal ra, hex32
    li t0, 0x20                     # ' '
    sb t0, PREFIX_LEN + 8(s5)
    mv a0, s1
    addi a1, s5, PREFIX_LEN + 9
    jal ra, hex32
    li t0, 0x0A                     # '\n'
    sb t0, LINE_LEN - 1(s5)
    li a0, 1
    mv a1, s5
    li a2, LINE_LEN
    li a7, SYS_WRITE
    ecall
    li a0, 0
    li a7, SYS_EXIT
    ecall

# poll: if the mailbox is full, takes its value into the digest and clears the flag.
poll:
    lw t0, FULL(s0)
    beqz t0, 1f
    lw t0, DATA(s0)
    slli t1, s3, 5
    sub s3, t1, s3
    add s3, s3, t0
    sw zero, FULL(s0)
    addi s4, s4, 1
1:  ret

# hex32: writes a0 as 8 lowercase hex digits at a1.
hex32:
    li t0, 8
1:  srli t1, a0, 28
    li t2, 10
    blt t1, t2, 2f
    addi t1, t1, 0x61 - 10 - 0x30   # 'a' - 10 - '0'
2:  addi t1, t1, 0x30               # '0'
    sb t1, 0(a1)
    slli a0, a0, 4
    addi a1, a1, 1
    addi t0, t0, -1
    bnez t0, 1b
    ret

    .section .rodata
prefix:
    .ascii "spinner "

    .section .bss
line:
    .zero 32
