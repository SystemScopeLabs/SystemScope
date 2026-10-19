# reader: entries 2 and 3 of the M5 reference disk (docs/m5-design.md §12.2). Reads its
# data block with blk_read (syscall 244, m5-design §11.3), the two instances contending
# for the one block controller: each one's first read is its first action after getpid,
# so the second request waits behind the first. After each read it requires a0 == 0
# (else exit 2) and every byte i to be (7 * i + lba) mod 256, the disk's data pattern
# (else exit 1), then clears the buffer for the next read. Then it prints
# "reader DDDDDDDD\n", the digest of every byte read (d = 31 * d + b) in hex, and exits 0.
#
# Entry 2 (pid 3) reads block 200 once, entry 3 (pid 4) block 201 twice: the LBA is
# DATA_LBA + pid - 3 and the number of reads pid - 2. The second reader's last read comes
# after the first reader has exited, so it can find every other process done.
#
# gp-independent (m3-design §17.1): linked without relaxation.

    .option norelax

    .equ SYS_WRITE, 64
    .equ SYS_EXIT, 93
    .equ SYS_GETPID, 172
    .equ SYS_BLK_READ, 244
    .equ DATA_LBA, 200
    .equ BLOCK, 512

    .equ PREFIX_LEN, 7              # "reader "
    .equ LINE_LEN, 16

    .section .text.init, "ax"
    .globl _start
_start:
    li a7, SYS_GETPID
    ecall
    addi s1, a0, DATA_LBA - 3       # lba
    addi s2, a0, -2                 # reads
    la s0, buffer
    li s3, 0                        # digest
1:  # Read the block into the buffer, zero from bss or cleared below.
    mv a0, s1
    mv a1, s0
    li a7, SYS_BLK_READ
    ecall
    bnez a0, eio
    # Check the pattern and digest the bytes.
    li t0, 0
    li t4, BLOCK
3:  add t1, s0, t0
    lbu t2, 0(t1)
    slli t3, t0, 3
    sub t3, t3, t0
    add t3, t3, s1
    andi t3, t3, 0xFF
    bne t2, t3, wrong
    slli t1, s3, 5
    sub s3, t1, s3
    add s3, s3, t2
    addi t0, t0, 1
    blt t0, t4, 3b
    addi s2, s2, -1
    beqz s2, 5f
    # Clear the buffer for the next read.
    mv t0, s0
    li t1, BLOCK / 4
2:  sw zero, 0(t0)
    addi t0, t0, 4
    addi t1, t1, -1
    bnez t1, 2b
    j 1b
5:  # The line: the prefix, the digest, a newline.
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
wrong:
    li a0, 1
    li a7, SYS_EXIT
    ecall
eio:
    li a0, 2
    li a7, SYS_EXIT
    ecall

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
    .ascii "reader "

    .section .bss
buffer:
    .zero BLOCK
line:
    .zero 16
