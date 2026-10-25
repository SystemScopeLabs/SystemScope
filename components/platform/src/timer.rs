//! `SimpleTimer`: a memory-mapped down-counter that raises an `irq.v0` line
//! (`docs/m5-design.md` §10).
//!
//! It never ticks. Arming it schedules one self-wake `COMPARE` cycles ahead on its clock
//! domain, so an idle or counting timer costs nothing until it expires. The remaining
//! count is not readable: software sees only whether the timer is enabled and whether it
//! has expired, never a timing value.
//!
//! # Ports
//!
//! In [`ports()`](Component::ports) order: `mem`, a `mem.v1` target, the MMIO window
//! ([`MEM_PORT`]); and `irq`, an `irq.v0` initiator ([`IRQ_PORT`]).
//!
//! # Register map
//!
//! Offsets from the timer's own start, as the bus forwards them; the window is [`SIZE`]
//! bytes.
//!
//! | Offset | Register | Access | Behavior |
//! |---|---|---|---|
//! | `0x0` | `COMPARE` | read/write, 4 bytes | the cycles from arming to expiry, at least 1; a write of 0 faults |
//! | `0x4` | `CONTROL` | read/write, 4 bytes | bit 0 `ENABLE`, bit 1 `PERIODIC`; other bits read 0 |
//! | `0x8` | `STATUS` | read/write-1-to-clear, 4 bytes | bit 0 `EXPIRED`, the line's level |
//!
//! Every other well-formed request gets a [`MemFault::AccessFault`] response and changes
//! nothing: a write of 0 to `COMPARE`, any width other than 4, any other offset, and a
//! request whose last byte would lie past `u64::MAX`. A zero-length request faults the
//! session, as for the other devices.
//!
//! # Lifecycle
//!
//! - **Arm.** A `CONTROL` write that sets `ENABLE` while it is clear arms the timer: it
//!   schedules a wake `COMPARE` cycles from the acceptance tick, in `Complete`. A write
//!   that keeps `ENABLE` set does not re-arm; a `COMPARE` write takes effect at the next
//!   arm.
//! - **Disarm.** A `CONTROL` write that clears `ENABLE` cancels the pending expiry. Each
//!   arm and disarm advances a generation, the wake's token, so a wake from an earlier
//!   generation is stale and ignored.
//! - **Expire.** At its wake, the timer sets `EXPIRED` and raises the line if it was low.
//!   A periodic timer re-arms `COMPARE` cycles from the expiry; a one-shot timer clears
//!   `ENABLE`.
//! - **Acknowledge.** Writing 1 to `STATUS` bit 0 clears `EXPIRED` and drops the line.
//!
//! The line is `EXPIRED`: the timer sends `Level` on `irq` (`Now`, `Complete`) exactly
//! when `EXPIRED` changes. It starts low, so nothing is sent until the first expiry.
//!
//! # Ordering semantics
//!
//! As for the RAM: a request takes effect when it is accepted, and its response follows
//! after the configured latency, in `Complete`.

use std::num::NonZeroU32;

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::{
    self, Access, MemFault, MemMsg, ReadOutcome, WriteOutcome,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;

/// Offset of the `COMPARE` register.
pub const COMPARE: u64 = 0x0;

/// Offset of the `CONTROL` register.
pub const CONTROL: u64 = 0x4;

/// Offset of the `STATUS` register.
pub const STATUS: u64 = 0x8;

/// The size of the timer's window: `COMPARE`, `CONTROL`, `STATUS`.
pub const SIZE: u64 = 12;

/// `CONTROL` bit 0: the timer is counting.
pub const ENABLE: u32 = 1 << 0;

/// `CONTROL` bit 1: the timer re-arms at expiry.
pub const PERIODIC: u32 = 1 << 1;

/// `STATUS` bit 0: the timer has expired and its line is high.
pub const EXPIRED: u32 = 1 << 0;

/// The `mem` port.
pub const MEM_PORT: PortId = PortId(0);

/// The `irq` port.
pub const IRQ_PORT: PortId = PortId(1);

/// The trace record of an arm, with `cycles` and `generation`.
pub const ARM_KIND: &str = "platform.timer.arm";

/// The trace record of an expiry, with `generation` and `periodic`.
pub const EXPIRE_KIND: &str = "platform.timer.expire";

/// Layout of [`SimpleTimer`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Clock and timing of a [`SimpleTimer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerConfig {
    /// The domain `COMPARE` counts on.
    pub clock: ClockDomainId,
    /// Delay from accepting a request to sending its response, as for
    /// [`RamConfig`](crate::RamConfig).
    pub latency: LinkLatency,
    /// `COMPARE` at reset.
    pub compare_reset: NonZeroU32,
}

/// A memory-mapped down-counter with an `irq.v0` line.
pub struct SimpleTimer {
    config: TimerConfig,
    /// `COMPARE`, at least 1.
    compare: u32,
    /// `CONTROL`: `ENABLE` and `PERIODIC` only.
    control: u32,
    /// `STATUS.EXPIRED`, and the level last sent on `irq`.
    expired: bool,
    /// Advanced at each arm and disarm; the token of the one live wake while enabled.
    generation: u64,
}

impl SimpleTimer {
    /// Creates a disabled timer, `COMPARE` at its reset value, the line low.
    pub fn new(config: TimerConfig) -> SimpleTimer {
        SimpleTimer {
            config,
            compare: config.compare_reset.get(),
            control: 0,
            expired: false,
            generation: 0,
        }
    }

    /// `COMPARE`.
    pub fn compare(&self) -> u32 {
        self.compare
    }

    /// `CONTROL`.
    pub fn control(&self) -> u32 {
        self.control
    }

    /// `STATUS.EXPIRED`, the line's level.
    pub fn expired(&self) -> bool {
        self.expired
    }

    /// The generation: the token of the live wake while enabled.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Advances the generation. A restored snapshot may carry any generation, so the
    /// last one faults instead of wrapping onto a stale wake's token.
    fn advance(&mut self) -> Result<(), SimError> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(SimError::ComponentFault("timer: generation exhausted"))?;
        Ok(())
    }

    /// Advances the generation and schedules its wake `COMPARE` cycles from now.
    fn arm(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.advance()?;
        ctx.trace(
            ARM_KIND,
            vec![
                ("cycles", Value::U64(u64::from(self.compare))),
                ("generation", Value::U64(self.generation)),
            ],
        );
        let when = ScheduleWhen::Cycles {
            domain: self.config.clock,
            k: u64::from(self.compare),
        };
        ctx.wake_self(when, Phase::Complete, self.generation)
    }

    /// Sets the line to `level`, sending it only if it changes.
    fn set_line(&mut self, level: bool, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if level == self.expired {
            return Ok(());
        }
        self.expired = level;
        ctx.send(
            IRQ_PORT,
            IrqMsg::Level { asserted: level }.into(),
            ScheduleWhen::Now,
            Phase::Complete,
        )
    }

    /// The wake with `token`: an expiry if it is the live generation's, else stale.
    fn wake(&mut self, token: u64, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if self.control & ENABLE == 0 || token != self.generation {
            return Ok(());
        }
        let periodic = self.control & PERIODIC != 0;
        ctx.trace(
            EXPIRE_KIND,
            vec![
                ("generation", Value::U64(self.generation)),
                ("periodic", Value::Bool(periodic)),
            ],
        );
        self.set_line(true, ctx)?;
        if periodic {
            self.arm(ctx)
        } else {
            self.control &= !ENABLE;
            Ok(())
        }
    }

    /// Writes `CONTROL`: arms on `ENABLE` 0 → 1, disarms on 1 → 0.
    fn write_control(&mut self, value: u32, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let was = self.control & ENABLE != 0;
        self.control = value & (ENABLE | PERIODIC);
        match (was, self.control & ENABLE != 0) {
            (false, true) => self.arm(ctx),
            (true, false) => self.advance(),
            _ => Ok(()),
        }
    }

    /// Accepts a request on `mem`: reads or writes a register now, and sends the response
    /// after the configured latency, in `Complete`.
    fn request(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let first = match msg.access() {
            None => {
                return Err(SimError::ComponentFault(
                    "timer: response on the target port",
                ));
            }
            Some(Access::Empty) => {
                return Err(SimError::ComponentFault("timer: zero-length request"));
            }
            Some(Access::OutOfRange) => None,
            Some(Access::Bytes { first, .. }) => Some(first),
        };
        let fault = MemFault::AccessFault;
        let resp = match msg {
            MemMsg::ReadReq { txn, len, .. } => {
                let value = match (first, *len) {
                    (Some(COMPARE), 4) => Some(self.compare),
                    (Some(CONTROL), 4) => Some(self.control),
                    (Some(STATUS), 4) => Some(u32::from(self.expired)),
                    _ => None,
                };
                MemMsg::ReadResp {
                    txn: *txn,
                    outcome: match value {
                        Some(v) => ReadOutcome::Data {
                            data: v.to_le_bytes().to_vec(),
                        },
                        None => ReadOutcome::Fault { fault },
                    },
                }
            }
            MemMsg::WriteReq { txn, data, .. } => {
                let value = <[u8; 4]>::try_from(data.as_slice()).map(u32::from_le_bytes);
                let done = match (first, value) {
                    (Some(COMPARE), Ok(v)) if v != 0 => {
                        self.compare = v;
                        true
                    }
                    (Some(CONTROL), Ok(v)) => {
                        self.write_control(v, ctx)?;
                        true
                    }
                    (Some(STATUS), Ok(v)) => {
                        if v & EXPIRED != 0 {
                            self.set_line(false, ctx)?;
                        }
                        true
                    }
                    _ => false,
                };
                MemMsg::WriteResp {
                    txn: *txn,
                    outcome: if done {
                        WriteOutcome::Done
                    } else {
                        WriteOutcome::Fault { fault }
                    },
                }
            }
            MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => {
                return Err(SimError::ComponentFault(
                    "timer: response on the target port",
                ));
            }
        };
        let when = match self.config.latency {
            LinkLatency::After(d) => ScheduleWhen::After(d),
            LinkLatency::Cycles { domain, k } => ScheduleWhen::Cycles { domain, k },
        };
        ctx.send(MEM_PORT, resp.into(), when, Phase::Complete)
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        w.u32(self.config.clock.0);
        match self.config.latency {
            LinkLatency::After(d) => {
                w.u8(0);
                w.u128(d.as_femtoseconds());
            }
            LinkLatency::Cycles { domain, k } => {
                w.u8(1);
                w.u32(domain.0);
                w.u64(k);
            }
        }
        w.u32(self.config.compare_reset.get());
    }
}

impl Component for SimpleTimer {
    fn type_name(&self) -> &'static str {
        "platform.timer"
    }

    /// `mem`, then `irq`.
    fn ports(&self) -> Vec<PortSpec> {
        vec![
            PortSpec {
                name: "mem",
                protocol: mem_v1::PROTOCOL,
                role: Role::Target,
            },
            PortSpec {
                name: "irq",
                protocol: irq_v0::PROTOCOL,
                role: Role::Initiator,
            },
        ]
    }

    /// Nothing to send or schedule: the timer starts disabled, its line low.
    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Wake { token } => self.wake(*token, ctx),
            Delivered::Message {
                port: MEM_PORT,
                msg: Message::MemV1(msg),
            } => self.request(msg, ctx),
            Delivered::Message { port: MEM_PORT, .. } => Err(SimError::ComponentFault(
                "timer: message on mem is not mem.v1",
            )),
            Delivered::Message { port: IRQ_PORT, .. } => {
                Err(SimError::ComponentFault("timer: message on the irq port"))
            }
            Delivered::Message { .. } => Err(SimError::ComponentFault(
                "timer: message on an unknown port",
            )),
        }
    }

    /// `compare`, `control`, `expired`, and `generation`.
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![
                ("compare", Value::U64(u64::from(self.compare))),
                ("control", Value::U64(u64::from(self.control))),
                ("expired", Value::Bool(self.expired)),
                ("generation", Value::U64(self.generation)),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the configuration (clock, latency, `compare_reset`), then `compare`,
    /// `control`, `expired`, and `generation`. A pending expiry is a runtime event and comes
    /// back with the runtime's queue.
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.u32(self.compare);
        w.u32(self.control);
        w.bool(self.expired);
        w.u64(self.generation);
    }

    /// Replaces the state with the snapshot's. Rejects a different configuration, a
    /// `COMPARE` of 0, and a `CONTROL` bit other than `ENABLE` and `PERIODIC`, which no
    /// run produces.
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let mut config = SnapshotWriter::new();
        self.write_config(&mut config);
        if r.raw(config.as_bytes().len())? != config.as_bytes() {
            return Err(RestoreError::InvalidState(
                "timer: snapshot has a different configuration",
            ));
        }
        let compare = r.u32()?;
        let control = r.u32()?;
        let expired = r.bool()?;
        let generation = r.u64()?;
        if compare == 0 {
            return Err(RestoreError::InvalidState("timer: COMPARE is 0"));
        }
        if control & !(ENABLE | PERIODIC) != 0 {
            return Err(RestoreError::InvalidState(
                "timer: CONTROL has a reserved bit",
            ));
        }
        self.compare = compare;
        self.control = control;
        self.expired = expired;
        self.generation = generation;
        Ok(())
    }
}
