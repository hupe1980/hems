//! SunSpec over Modbus TCP, without a socket in sight.
//!
//! The **device** driver: an inverter, a meter or a battery that a
//! household already owns, read over the one protocol that needs no membership,
//! no registration and no certificate. Most inverters sold in Germany speak it,
//! and it is where a box that manages a real house starts.
//!
//! # What is ours and what is not
//!
//! The register maps are **not** ours. SunSpec is a thousand pages of model
//! definitions, and the [`sunspec`] crate carries them as generated types with
//! `Model::parse` — a pure function from a register block to a typed struct.
//! Retyping them here would be the same mistake as re-implementing the EEBUS
//! state machine: a second copy that disagrees, and the wrong one is whichever
//! nobody is testing.
//!
//! What is ours is the part a specification cannot give you: the framing
//! ([`frame`]), the walk that finds the models on a particular device, the
//! decision about what a register block *means* for a household, and the
//! honesty about what this protocol cannot say.
//!
//! # What SunSpec cannot say, and why that is reported rather than guessed
//!
//! A curtailed inverter asked what it is producing answers with what the manager
//! already commanded. Read that alone and a controller never lifts its own
//! curtailment: it asks for 5 kW, reads 5 kW, and concludes the roof is doing
//! its best.
//!
//! The common inverter models (101/103) have no way out of that — they publish
//! what is flowing and nothing about what could. Model **701** does: `ThrotPct`
//! is how much throttling is in effect, so `W / (1 − ThrotPct)` recovers what
//! the array would deliver unthrottled. A device that publishes 701 therefore
//! gets [`DriverCapabilities::reports_available_power`] and one that does not,
//! does not — and `hemsd` falls back to the nameplate knowing that it is.
//!
//! That flag is the whole point of declaring capabilities rather than assuming
//! them: a household is entitled to know which of the two its box is running on.

mod decode;
pub mod frame;
mod scan;

pub use scan::{Discovery, ModelMap};

use crate::{CommandOutcome, Driver, DriverCapabilities, DriverError, DriverEvent, LinkState};
use hems_core::prelude::{AssetId, Measurement, Power};
use hems_core::setpoint::Command;
use time::{Duration, OffsetDateTime};

use frame::{Request, RequestBody, Response, ResponseBody};

/// What kind of thing is on the other end.
///
/// Decided by which models the device publishes rather than by configuration: a
/// device that carries model 103 is an inverter whatever a TOML file calls it,
/// and a mismatch between the two is worth finding at discovery rather than in a
/// measurement that reads plausibly and means something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A photovoltaic inverter — models 101/102/103, or 701.
    Inverter,
    /// A meter — models 201–204.
    Meter,
    /// A battery — model 802.
    Battery,
}

/// How often to poll, and how long to wait for an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    /// How often a full read of the interesting models is issued.
    ///
    /// A second is what the guard's control period wants; a slow inverter over a
    /// serial gateway may need more. It is a *floor*: the driver does not start
    /// a new poll while one is outstanding, so a device that answers in two
    /// seconds is polled every two.
    pub poll: Duration,
    /// How long an unanswered request may stand before the link is stale.
    ///
    /// Not a retry: this is the point at which the guard should stop believing
    /// the last measurement. It has to be shorter than the guard's own
    /// `max_measurement_age`, or a device could be silent for a whole control
    /// period while the guard still counted its last reading as fresh.
    pub timeout: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            poll: Duration::seconds(1),
            timeout: Duration::seconds(5),
        }
    }
}

/// A SunSpec device on the other end of a Modbus TCP connection.
#[derive(Debug)]
pub struct SunSpec {
    asset: AssetId,
    unit: u8,
    cadence: Cadence,
    /// Where the model list starts. SunSpec allows three, and a device answers
    /// on exactly one of them.
    discovery: Discovery,
    models: ModelMap,
    kind: Option<Kind>,
    link: LinkState,
    /// Bytes that have arrived and are not yet a whole frame.
    inbox: Vec<u8>,
    /// Requests waiting to go out.
    outbox: Vec<Request>,
    /// What was asked, and when — so an answer can be matched and a silence
    /// noticed.
    pending: Option<Pending>,
    next_transaction: u16,
    events: Vec<DriverEvent>,
    /// When the next poll is due.
    due: Option<OffsetDateTime>,
    /// The measurement being assembled from several models this round.
    partial: Option<Measurement>,
    /// Whether this driver is configured to read and never command.
    listens_only: bool,
}

/// A request that has gone out and not yet come back.
#[derive(Debug, Clone)]
struct Pending {
    transaction: u16,
    at: OffsetDateTime,
    purpose: Purpose,
}

/// Why a request was sent, so its answer can be understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// Looking for the `SunS` marker at one of the three base addresses.
    Marker(u16),
    /// Reading a model header while walking the chain.
    Header(u16),
    /// Reading the models that carry a measurement.
    Poll { id: u16, address: u16 },
    /// Writing a curtailment setpoint.
    Curtail,
}

impl SunSpec {
    /// Read this device and never command it.
    ///
    /// A meter is the ordinary case, and so is an inverter behind a vendor
    /// gateway that exposes SunSpec read-only. The distinction is *site
    /// configuration* rather than something to discover: the alternative is a
    /// driver that claims it can command until a model walk says otherwise,
    /// which is a claim nothing can check at the moment it matters.
    #[must_use]
    pub fn listening_only(mut self) -> Self {
        self.listens_only = true;
        self
    }

    /// The device's rated power, which curtailment is expressed against.
    ///
    /// SunSpec model 123 curtails in **per cent of `WMax`**, so a ceiling in
    /// watts cannot be written without it. It comes from the site rather than
    /// from the wire because the site already knows it — an `Asset` carries its
    /// connection power — and because the nameplate models that would publish it
    /// are optional, so a driver that only read them would refuse to curtail
    /// perfectly ordinary inverters.
    ///
    /// Without it [`Driver::command`] refuses a production ceiling rather than
    /// writing a percentage computed from a guess.
    #[must_use]
    pub fn with_rating(mut self, rating: Power) -> Self {
        self.models.set_rating(rating);
        self
    }

    /// A device at `unit` behind a Modbus TCP connection.
    pub fn new(asset: AssetId, unit: u8, cadence: Cadence) -> Self {
        Self {
            asset,
            unit,
            cadence,
            discovery: Discovery::new(),
            models: ModelMap::default(),
            kind: None,
            link: LinkState::Down,
            inbox: Vec::new(),
            outbox: Vec::new(),
            pending: None,
            next_transaction: 1,
            events: Vec::new(),
            due: None,
            partial: None,
            listens_only: false,
        }
    }

    /// What the device turned out to be, once discovery has finished.
    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }

    /// Which models the device publishes.
    pub fn models(&self) -> &ModelMap {
        &self.models
    }

    /// The next transaction identifier.
    ///
    /// Wraps, and skips zero: some gateways treat a zero transaction as "no
    /// correlation" and will answer it out of order.
    fn transaction(&mut self) -> u16 {
        self.next_transaction = self.next_transaction.wrapping_add(1).max(1);
        self.next_transaction
    }

    /// Queue a read.
    fn read(&mut self, address: u16, count: u16, purpose: Purpose, now: OffsetDateTime) {
        let transaction = self.transaction();
        self.outbox.push(Request {
            transaction,
            unit: self.unit,
            body: RequestBody::Read { address, count },
        });
        self.pending = Some(Pending {
            transaction,
            at: now,
            purpose,
        });
    }

    /// Take the next step of whatever the driver is in the middle of.
    fn advance(&mut self, now: OffsetDateTime) {
        if self.pending.is_some() {
            return;
        }
        // Discovery first: nothing can be read until the model list is known.
        if let Some((address, count, purpose)) = self.discovery.next_step(&self.models) {
            self.read(address, count, purpose, now);
            return;
        }
        if self.link != LinkState::Up {
            self.link = LinkState::Up;
            self.kind = Some(self.models.kind());
            self.events.push(DriverEvent::Link(LinkState::Up));
        }
        // Then the poll, when it is due.
        if self.due.is_none_or(|due| now >= due) {
            if let Some((id, address, len)) = self.models.next_poll() {
                self.partial.get_or_insert_with(|| Measurement::at(now));
                self.read(address, len, Purpose::Poll { id, address }, now);
            } else {
                self.finish_poll(now);
            }
        }
    }

    /// Emit whatever the round assembled and schedule the next one.
    fn finish_poll(&mut self, now: OffsetDateTime) {
        if let Some(measurement) = self.partial.take() {
            self.events.push(DriverEvent::Measured(measurement));
        }
        self.models.rewind();
        self.due = Some(now + self.cadence.poll);
    }
}

impl Driver for SunSpec {
    fn asset(&self) -> &AssetId {
        &self.asset
    }

    fn capabilities(&self) -> DriverCapabilities {
        // What the driver is **for**, not what it has discovered. Registration
        // happens before the first byte, so a capability that only became true
        // after a model walk would be a capability nothing could check — and the
        // check is the whole reason the type exists. The role comes from the
        // site, which already knows whether this address is an inverter or a
        // meter, and discovery is not consulted about it.
        let base = if self.listens_only {
            DriverCapabilities::meter()
        } else {
            DriverCapabilities::device()
        };
        // Only a device that publishes model 701 can say what it *could*
        // produce; see the module note. Guessing here would be the difference
        // between a curtailment that lifts and one that does not.
        if self.models.reports_available_power() {
            base.with_available_power()
        } else {
            base
        }
    }

    fn on_bytes(&mut self, bytes: &[u8], now: OffsetDateTime) -> Result<(), DriverError> {
        self.inbox.extend_from_slice(bytes);
        loop {
            match frame::decode(&self.inbox) {
                Ok(None) => break,
                Ok(Some((response, used))) => {
                    self.inbox.drain(..used);
                    self.handle(&response, now);
                }
                Err(e) => {
                    // A frame this driver cannot parse means the stream is no
                    // longer aligned; keeping the rest would decode rubbish at
                    // an offset. Dropping it is the only honest recovery.
                    self.inbox.clear();
                    self.pending = None;
                    return Err(DriverError::Malformed(e.to_string()));
                }
            }
        }
        self.advance(now);
        Ok(())
    }

    fn on_timeout(&mut self, now: OffsetDateTime) {
        // An unanswered request is the only thing a timeout means here. The
        // guard is told the link is stale rather than being left with a
        // measurement that looks fresh because nothing contradicted it.
        if let Some(p) = self.pending.clone()
            && now - p.at >= self.cadence.timeout
        {
            self.pending = None;
            self.partial = None;
            if self.link != LinkState::Stale {
                self.link = LinkState::Stale;
                self.events.push(DriverEvent::Link(LinkState::Stale));
            }
            self.discovery.restart();
            self.models.rewind();
        }
        self.advance(now);
    }

    fn command(&mut self, command: &Command, now: OffsetDateTime) -> Result<(), DriverError> {
        let Command::ProductionCeiling(ceiling) = command else {
            return Err(DriverError::Unsupported(format!("{command:?}")));
        };
        let Some(write) = self.models.curtailment_write(*ceiling) else {
            return Err(DriverError::Unsupported(
                "a production ceiling on a device with no model 123".into(),
            ));
        };
        if self.link != LinkState::Up {
            return Err(DriverError::NotConnected);
        }
        let transaction = self.transaction();
        self.outbox.push(Request {
            transaction,
            unit: self.unit,
            body: RequestBody::Write {
                address: write.address,
                values: write.values,
            },
        });
        self.pending = Some(Pending {
            transaction,
            at: now,
            purpose: Purpose::Curtail,
        });
        Ok(())
    }

    fn poll_event(&mut self) -> Option<DriverEvent> {
        if self.events.is_empty() {
            None
        } else {
            Some(self.events.remove(0))
        }
    }

    fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        if self.outbox.is_empty() {
            None
        } else {
            Some(self.outbox.remove(0).encode())
        }
    }

    fn poll_deadline(&self) -> Option<OffsetDateTime> {
        // Whichever comes first: an outstanding request giving up, or the next
        // poll falling due. Never `None` — a driver that offered no deadline
        // would be woken only by bytes, and a silent device sends none.
        let timeout = self.pending.as_ref().map(|p| p.at + self.cadence.timeout);
        match (timeout, self.due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

impl SunSpec {
    /// Fold one answer into whatever it was asked for.
    fn handle(&mut self, response: &Response, now: OffsetDateTime) {
        let Some(pending) = self.pending.clone() else {
            // An answer to nothing: a late reply to a request already given up
            // on. Dropping it is right — folding it in would put a stale reading
            // into a fresh measurement.
            return;
        };
        if pending.transaction != response.transaction {
            return;
        }
        self.pending = None;

        match (&pending.purpose, &response.body) {
            // An exception while walking is how a device says "no model there".
            (_, ResponseBody::Exception { .. }) => {
                self.discovery.refused(pending.purpose, &mut self.models);
            }
            (Purpose::Marker(base), ResponseBody::Registers(regs)) => {
                self.discovery.saw_marker(*base, regs, &mut self.models);
            }
            (Purpose::Header(address), ResponseBody::Registers(regs)) => {
                self.discovery.saw_header(*address, regs, &mut self.models);
            }
            (Purpose::Poll { id, .. }, ResponseBody::Registers(regs)) => {
                if let Some(m) = self.partial.as_mut() {
                    decode::fold(*id, regs, m);
                }
                self.models.advance_poll();
            }
            (Purpose::Curtail, ResponseBody::WriteAccepted { .. }) => {
                self.events.push(DriverEvent::Command(CommandOutcome {
                    accepted: true,
                    confirmed: None,
                    at: now,
                    detail: None,
                }));
            }
            _ => {}
        }
    }
}

/// A curtailment write, resolved to registers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurtailWrite {
    /// The first register.
    pub address: u16,
    /// What to put there.
    pub values: Vec<u16>,
}

/// What a ceiling means as a percentage of the device's rating.
///
/// SunSpec model 123 curtails in **per cent of `WMax`**, not in watts, so a
/// driver that does not know the rating cannot express a ceiling at all — and
/// saying so is better than writing a percentage computed from a guess.
#[must_use]
pub fn curtail_percent(ceiling: Power, rating: Power) -> Option<u16> {
    if rating <= Power::ZERO {
        return None;
    }
    let pct = (ceiling.get() / rating.get() * 100.0).clamp(0.0, 100.0);
    // Clamped to `[0, 100]` on the line above and rounded here, so every value
    // it can take is a whole number a `u16` holds exactly.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to [0, 100] and rounded, so the conversion is exact"
    )]
    Some(pct.round() as u16)
}
