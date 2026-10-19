# counter: entries 0 and 1 of the M5 reference disk (docs/m5-design.md §12.2). Two
# instances each add 1 to the shared counter ROUNDS times, each increment inside a
# Peterson critical section on the shared page, with WORK iterations of private work
# between increments; then each exits 0. The final counter is 2 * ROUNDS only because the
# lock is correct. It prints nothing; the counter is dumped at shutdown.
#
# A waiting instance withdraws (flag[me] = 0), yields, and enters again, instead of
# spinning: the other one holds the lock only when it was preempted inside it, so spinning
# would burn the rest of the slice, and waiting with the flag set would hand the lock back
# and forth one increment per yield. Withdrawing keeps mutual exclusion: each entry is a
# whole Peterson entry.
#
# The instance is getpid() - 1: entry 0 is pid 1, entry 1 is pid 2. Any other pid exits 2.
#
# gp-independent (m3-design §17.1): linked without relaxation, every address is
# pc-relative or absolute.

    .option norelax

    .equ SYS_EXIT, 93
    .equ SYS_SCHED_YIELD, 124
    .equ SYS_GETPID, 172
    .equ ROUNDS, 150
    .equ WORK, 8

    # The shared page (m5-design §11.4) and its words.
    .equ SHARED_VA, 0xF000
    .equ COUNTER, 0x0
    .equ FLAG, 0xC                  # flag[0]; flag[1] is the next word
    .equ TURN, 0x14

    .section .text.init, "ax"
    .globl _start
_start:
    li a7, SYS_GETPID
    ecall
    addi s1, a0, -1                 # me
    li t0, 2
    bgeu s1, t0, bad
    xori s2, s1, 1                  # other
    li s0, SHARED_VA
    slli t0, s1, 2
    add s3, s0, t0                  # s3 + FLAG = &flag[me]
    slli t0, s2, 2
    add s4, s0, t0                  # s4 + FLAG = &flag[other]
    li s5, ROUNDS
    li s6, 1
1:  # flag[me] = 1; turn = other; enter unless flag[other] && turn == other.
    sw s6, FLAG(s3)
    sw s2, TURN(s0)
    lw t0, FLAG(s4)
    beqz t0, 3f
    lw t0, TURN(s0)
    bne t0, s2, 3f
    sw zero, FLAG(s3)
    li a7, SYS_SCHED_YIELD
    ecall
    j 1b
3:  # The critical section.
    lw t0, COUNTER(s0)
    addi t0, t0, 1
    sw t0, COUNTER(s0)
    sw zero, FLAG(s3)
    # Private work.
    li t1, WORK
4:  addi t1, t1, -1
    bnez t1, 4b
    addi s5, s5, -1
    bnez s5, 1b
    li a0, 0
    li a7, SYS_EXIT
    ecall
bad:
    li a0, 2
    li a7, SYS_EXIT
    ecall
