//! The planted races of `m5-reference` (`docs/m5-design.md` §12.3). R1–R3 each patch one
//! program on the disk, as an M4 media patch: two instructions, or an instruction and a
//! group of three, trade places. R5 plants a defect in the kernel. There is no R4
//! (§27.7).
//!
//! Every patch is found by its instructions, not by a fixed offset, inside the program's
//! extent as the disk's table lists it, so a rebuilt workload moves it with the code.

use systemscope_elf::{BLOCK_SIZE, parse_exec_table};
use systemscope_os::Defect;
use systemscope_rv32::m3ref::{DISK_BLOCKS, STAGING_SIZE};
use systemscope_verify::case::{InputVariant, MediaPatch};

/// A planted race.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Race {
    /// `counter-unlocked`: entry 0 releases the lock before its increment.
    R1,
    /// `peterson-turn`: entry 0 enters when `turn` names the other instance, the test
    /// inverted, so it enters while the other holds the lock.
    R2,
    /// `mailbox-data-first`: `spinner` loads the mailbox data before it tests the flag,
    /// so it may take the data from before `handoff`'s last send.
    R3,
    /// `kernel-wake-lost`: [`Defect::IdleTickDropsWake`].
    R5,
}

/// The general registers the patches name.
const ZERO: u32 = 0;
const T0: u32 = 5;
const S0: u32 = 8;
const T1: u32 = 6;
const S3: u32 = 19;

fn i_type(op: u32, funct3: u32, rd: u32, rs1: u32, imm: i32) -> u32 {
    (imm as u32 & 0xfff) << 20 | rs1 << 15 | funct3 << 12 | rd << 7 | op
}

fn lw(rd: u32, imm: i32, rs1: u32) -> u32 {
    i_type(0x03, 2, rd, rs1, imm)
}

fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
    i_type(0x13, 0, rd, rs1, imm)
}

/// A branch's offset from its own address.
fn branch_offset(word: u32) -> i32 {
    let imm = (word >> 31 & 1) << 12
        | (word >> 7 & 1) << 11
        | (word >> 25 & 0x3f) << 5
        | (word >> 8 & 0xf) << 1;
    ((imm << 19) as i32) >> 19
}

fn beq(rs1: u32, rs2: u32, offset: i32) -> u32 {
    let imm = offset as u32;
    (imm >> 12 & 1) << 31
        | (imm >> 5 & 0x3f) << 25
        | rs2 << 20
        | rs1 << 15
        | (imm >> 1 & 0xf) << 8
        | (imm >> 11 & 1) << 7
        | 0x63
}

fn sw(rs2: u32, imm: i32, rs1: u32) -> u32 {
    let imm = imm as u32 & 0xfff;
    (imm >> 5) << 25 | rs2 << 20 | rs1 << 15 | 2 << 12 | (imm & 0x1f) << 7 | 0x23
}

fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// The byte ranges of the disk's executables, in table order.
fn extents(disk: &[u8]) -> Result<Vec<std::ops::Range<usize>>, String> {
    let block0: &[u8; BLOCK_SIZE] = disk
        .get(..BLOCK_SIZE)
        .and_then(|b| b.try_into().ok())
        .ok_or("the disk has no block 0")?;
    let table = parse_exec_table(block0, DISK_BLOCKS as u32, STAGING_SIZE as u32)
        .map_err(|e| format!("the disk's table: {e}"))?;
    Ok(table
        .entries
        .iter()
        .map(|e| {
            let at = e.start_lba as usize * BLOCK_SIZE;
            at..at + e.byte_len as usize
        })
        .collect())
}

/// The offset of the only occurrence of `pattern` in `entry`'s extent.
fn find(disk: &[u8], entry: usize, pattern: &[u8]) -> Result<usize, String> {
    let extent = extents(disk)?
        .get(entry)
        .cloned()
        .ok_or(format!("the disk has no entry {entry}"))?;
    let found: Vec<usize> = disk[extent.clone()]
        .windows(pattern.len())
        .enumerate()
        .filter(|(_, w)| *w == pattern)
        .map(|(i, _)| extent.start + i)
        .collect();
    match found[..] {
        [at] => Ok(at),
        _ => Err(format!(
            "entry {entry}: the patched instructions occur {} times",
            found.len()
        )),
    }
}

/// The patch from `old` to `new` at `at`.
fn patch(disk: &[u8], at: usize, new: &[u8]) -> MediaPatch {
    MediaPatch {
        offset: at as u64,
        old: disk[at..at + new.len()].to_vec(),
        new: new.to_vec(),
    }
}

impl Race {
    /// Every planted race.
    pub const ALL: [Race; 4] = [Race::R1, Race::R2, Race::R3, Race::R5];

    /// The name §12.3 gives it.
    pub fn name(self) -> &'static str {
        match self {
            Race::R1 => "counter-unlocked",
            Race::R2 => "peterson-turn",
            Race::R3 => "mailbox-data-first",
            Race::R5 => "kernel-wake-lost",
        }
    }

    /// The kernel defect it plants, if any.
    pub fn defect(self) -> Option<Defect> {
        (self == Race::R5).then_some(Defect::IdleTickDropsWake)
    }

    /// The input variant that patches `disk`: a media patch for R1–R3, none for R5.
    pub fn variant(self, disk: &[u8]) -> Result<InputVariant, String> {
        // counter's shared words (`counter.S`) and handoff's mailbox (`handoff.S`).
        const COUNTER: i32 = 0x0;
        const FLAG: i32 = 0xC;
        const TURN: i32 = 0x14;
        const DATA: i32 = 0x4;
        const FULL: i32 = 0x8;
        let p = match self {
            Race::R1 => {
                let increment = [lw(T0, COUNTER, S0), addi(T0, T0, 1), sw(T0, COUNTER, S0)];
                let release = sw(ZERO, FLAG, S3);
                let old = bytes(&[increment[0], increment[1], increment[2], release]);
                let at = find(disk, 0, &old)?;
                patch(
                    disk,
                    at,
                    &bytes(&[release, increment[0], increment[1], increment[2]]),
                )
            }
            Race::R2 => {
                // `lw t0, TURN(s0)` then `bne t0, s2, 3f`: the branch becomes `beq`.
                let load = bytes(&[lw(T0, TURN, S0)]);
                let at = find(disk, 0, &load)? + load.len();
                let bne = u32::from_le_bytes(disk[at..at + 4].try_into().expect("4 bytes"));
                if bne & 0x707f != 0x1063 {
                    return Err(format!(
                        "counter: {bne:#010x} after the turn load is no bne"
                    ));
                }
                patch(disk, at, &(bne & !0x7000).to_le_bytes())
            }
            Race::R3 => {
                // `poll` opens with `lw t0, FULL(s0)`, `beqz t0, 1f`, `lw t0, DATA(s0)`.
                // The data load moves first, the flag goes to t1, and the branch, one
                // instruction later, keeps its target.
                let flag = lw(T0, FULL, S0);
                let at = find(disk, 4, &bytes(&[flag]))?;
                let word = |i: usize| {
                    u32::from_le_bytes(disk[at + 4 * i..at + 4 * i + 4].try_into().expect("4"))
                };
                let (beqz, data) = (word(1), word(2));
                if beqz & 0x01ff_f07f != 0x0002_8063 || data != lw(T0, DATA, S0) {
                    return Err(format!("spinner: poll opens {beqz:#010x} {data:#010x}"));
                }
                let target = branch_offset(beqz) - 4;
                patch(
                    disk,
                    at,
                    &bytes(&[data, lw(T1, FULL, S0), beq(T1, ZERO, target)]),
                )
            }
            Race::R5 => return Ok(InputVariant::None),
        };
        Ok(InputVariant::Media(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use systemscope_rv32i::decode::decode;

    #[test]
    fn the_encoders_agree_with_the_decoder() {
        for (word, text) in [
            (
                lw(T0, 0, S0),
                "Load { op: W, rd: Reg(5), rs1: Reg(8), offset: 0 }",
            ),
            (
                addi(T0, T0, 1),
                "OpImm { op: Addi, rd: Reg(5), rs1: Reg(5), imm: 1 }",
            ),
            (
                sw(ZERO, 0xC, S3),
                "Store { op: W, rs1: Reg(19), rs2: Reg(0), offset: 12 }",
            ),
            (
                beq(T1, ZERO, 24),
                "Branch { op: Eq, rs1: Reg(6), rs2: Reg(0), offset: 24 }",
            ),
        ] {
            assert_eq!(format!("{:?}", decode(word).unwrap()), text);
        }
        assert_eq!(sw(T0, 0xC, S3), 0x0059_a623);
        for offset in [-4096, -8, 8, 24, 4094] {
            assert_eq!(branch_offset(beq(T1, ZERO, offset)), offset);
        }
    }
}
