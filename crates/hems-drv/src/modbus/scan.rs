//! Finding out what is on the other end.
//!
//! A SunSpec device does not publish a catalogue. It publishes a **chain**: the
//! four bytes `SunS` at one of three base addresses, then a run of
//! `(id, length, body…)` blocks ending at a sentinel. Walking it is the only way
//! to learn where model 103 lives on *this* inverter, because the answer differs
//! between manufacturers and between firmware versions of the same one.
//!
//! Two consequences a driver has to respect.
//!
//! *The base address is one of three and must be probed.* 40 000 is the common
//! one, 50 000 and 0 are the others, and a device answers on exactly one. Probing
//! is not a fallback: reading a model header from the wrong base returns whatever
//! that register happens to hold, which decodes as a plausible model id.
//!
//! *An exception ends the walk.* A device that has run out of models answers a
//! read past the end with `0x02`, illegal data address, rather than with a
//! sentinel — so the walk has to treat a refusal as an ending and not as a
//! fault.

use super::Purpose;
use hems_core::prelude::Power;
use std::collections::BTreeMap;

/// The three addresses a SunSpec device may start at.
pub const BASES: [u16; 3] = [40_000, 50_000, 0];

/// `SunS`, as two registers.
const MARKER: [u16; 2] = [0x5375, 0x6e53];

/// The identifier that ends the model chain.
const END_OF_CHAIN: u16 = 0xffff;

/// Where each model lives on one device.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ModelMap {
    /// Model id → (address of the body, length in registers).
    found: BTreeMap<u16, (u16, u16)>,
    /// The device's rated power, from the site's own configuration.
    rating: Option<Power>,
    /// Which model the current poll round is on.
    cursor: usize,
}

/// The models worth reading every round, in the order a measurement wants them.
///
/// Deliberately short. A poll that read every model a device publishes would
/// spend a control period on nameplate data that changes once a decade, and the
/// guard is waiting.
const POLLED: [u16; 6] = [103, 102, 101, 203, 802, 701];

impl ModelMap {
    /// Note a model at an address.
    pub(crate) fn insert(&mut self, id: u16, address: u16, len: u16) {
        let _ = self.found.insert(id, (address, len));
    }

    /// Whether the device publishes a model.
    #[must_use]
    pub fn has(&self, id: u16) -> bool {
        self.found.contains_key(&id)
    }

    /// Every model found, as `(id, address, length)`.
    pub fn iter(&self) -> impl Iterator<Item = (u16, u16, u16)> + '_ {
        self.found.iter().map(|(id, (a, l))| (*id, *a, *l))
    }

    /// What the device is, from what it publishes.
    #[must_use]
    pub fn kind(&self) -> super::Kind {
        if self.has(802) {
            super::Kind::Battery
        } else if (201..=204).any(|id| self.has(id)) {
            super::Kind::Meter
        } else {
            super::Kind::Inverter
        }
    }

    /// Whether this device can say what it *could* produce.
    ///
    /// Model 701 carries `ThrotPct`, and with it `W / (1 − ThrotPct)` recovers
    /// the unthrottled figure. Nothing else in the common model set can, so a
    /// device without 701 reports `false` and the caller falls back to a
    /// nameplate knowing that it has.
    #[must_use]
    pub fn reports_available_power(&self) -> bool {
        self.has(701)
    }

    /// The device's rated power, if the site told the driver.
    #[must_use]
    pub fn rating(&self) -> Option<Power> {
        self.rating
    }

    /// Tell the driver what the device is rated at.
    pub(crate) fn set_rating(&mut self, rating: Power) {
        self.rating = Some(rating);
    }

    /// The next model to read this round.
    #[must_use]
    pub fn next_poll(&self) -> Option<(u16, u16, u16)> {
        POLLED
            .iter()
            .skip(self.cursor)
            .find_map(|id| self.found.get(id).map(|(a, l)| (*id, *a, *l)))
    }

    /// Move past the model just read.
    pub(crate) fn advance_poll(&mut self) {
        // Step to just after whichever entry `next_poll` would have returned, so
        // a device that publishes only 203 does not re-read it for ever.
        let from = self.cursor;
        for (i, id) in POLLED.iter().enumerate().skip(from) {
            if self.found.contains_key(id) {
                self.cursor = i + 1;
                return;
            }
        }
        self.cursor = POLLED.len();
    }

    /// Start the next round.
    pub(crate) fn rewind(&mut self) {
        self.cursor = 0;
    }

    /// The registers that express a production ceiling, if the device can take one.
    ///
    /// Model 123's `WMaxLimPct` is a **percentage of the rating**, so a device
    /// that has not published a rating cannot be curtailed in watts and says so
    /// rather than being sent a percentage computed from a guess.
    #[must_use]
    pub fn curtailment_write(&self, ceiling: Power) -> Option<super::CurtailWrite> {
        let (address, _) = *self.found.get(&123)?;
        let percent = super::curtail_percent(ceiling, self.rating()?)?;
        // `WMaxLimPct` is the first point of the model body, and `WMaxLim_Ena`
        // the fifth. Writing the value without enabling it is the classic way to
        // curtail nothing at all and believe otherwise, so both go in one write.
        Some(super::CurtailWrite {
            address,
            values: vec![percent, 0, 0, 0, 1],
        })
    }
}

/// The walk that finds the models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Probing base address `BASES[i]` for the marker.
    Probing(usize),
    /// Reading the header of the model at `address`, `seen` models in.
    Walking {
        /// Where the next header should be.
        address: u16,
        /// How many headers this walk has already taken, so a device cannot
        /// drive it for ever. See [`MAX_MODELS`].
        seen: u16,
    },
    /// The chain has been walked.
    Done,
}

/// The most models one walk will follow before giving up on the chain.
///
/// Every step is driven by two registers the **device** chose — an identifier
/// and a length — and the next header is `address + 2 + length`, so the device
/// decides how long the walk is. A length of zero advances it two registers a
/// step and never reaches the sentinel; a length that overflows the address
/// space pins it at the top. Neither is dangerous (a driver that never finishes
/// discovery reports no models, and the guard falls back to a nameplate) and
/// both are a box that reads one device for ever and never manages it. The same
/// reasoning as `super::decode::scaled`: bound what hardware nobody here
/// controls can ask for (D128).
///
/// Sixty-four is far above anything real; the largest device this workspace has
/// met publishes eleven. Stopping needs no log line to be visible — a truncated
/// walk declares fewer [`crate::DriverCapabilities`], and `/v1/status` already
/// names an asset whose driver cannot say what it could produce.
const MAX_MODELS: u16 = 64;

impl Default for Discovery {
    fn default() -> Self {
        Self::new()
    }
}

impl Discovery {
    /// A walk that has not started.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::Probing(0),
        }
    }

    /// Whether the model list is known.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// Begin again — a device that went quiet may come back as something else.
    pub fn restart(&mut self) {
        self.state = State::Probing(0);
    }

    /// The next read the walk wants, if it is not finished.
    #[must_use]
    pub(crate) fn next_step(&self, _models: &ModelMap) -> Option<(u16, u16, Purpose)> {
        match self.state {
            State::Probing(i) => {
                let base = *BASES.get(i)?;
                Some((base, 2, Purpose::Marker(base)))
            }
            // A header is two registers: the model id and its length.
            State::Walking { address, .. } => Some((address, 2, Purpose::Header(address))),
            State::Done => None,
        }
    }

    /// The device answered the marker probe.
    pub(crate) fn saw_marker(&mut self, base: u16, regs: &[u16], _models: &mut ModelMap) {
        if regs.len() >= 2 && regs[0] == MARKER[0] && regs[1] == MARKER[1] {
            // The chain starts immediately after the marker.
            self.state = State::Walking {
                address: base + 2,
                seen: 0,
            };
        } else {
            self.try_next_base(base);
        }
    }

    /// The device answered a model header.
    pub(crate) fn saw_header(&mut self, address: u16, regs: &[u16], models: &mut ModelMap) {
        let seen = match self.state {
            State::Walking { seen, .. } => seen,
            _ => 0,
        };
        let (Some(&id), Some(&len)) = (regs.first(), regs.get(1)) else {
            self.state = State::Done;
            return;
        };
        // Three ways a chain ends, and only the first is the tidy one.
        //
        // The **sentinel** is what a well-behaved device sends. A **zero-length**
        // header is not a model: the body is what a model is, and a header
        // claiming none of it advances the walk by two registers for ever. And a
        // chain that would run off the end of the address space, or that has
        // already produced more models than any real device publishes, is a
        // device answering something other than the question — see
        // [`MAX_MODELS`].
        let next = address.checked_add(2).and_then(|a| a.checked_add(len));
        let (Some(next), false, true) = (next, id == END_OF_CHAIN, len > 0) else {
            self.state = State::Done;
            return;
        };
        if seen >= MAX_MODELS {
            self.state = State::Done;
            return;
        }
        // The body sits after the two-register header, and the next header
        // after the body.
        models.insert(id, address + 2, len);
        self.state = State::Walking {
            address: next,
            seen: seen + 1,
        };
    }

    /// The device refused a read.
    ///
    /// While probing, that is "not this base". While walking, it is the end of
    /// the chain: a device that has run out of models answers `0x02` rather than
    /// producing the sentinel.
    pub(crate) fn refused(&mut self, purpose: Purpose, _models: &mut ModelMap) {
        match purpose {
            Purpose::Marker(base) => self.try_next_base(base),
            _ => self.state = State::Done,
        }
    }

    fn try_next_base(&mut self, base: u16) {
        let next = BASES.iter().position(|b| *b == base).map_or(0, |i| i + 1);
        self.state = if next < BASES.len() {
            State::Probing(next)
        } else {
            // Nothing answered on any base. Starting again is the only useful
            // behaviour: the device may be booting.
            State::Probing(0)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk a chain whose headers come from `answer`, and say how many reads it
    /// took to finish. `None` means it did not.
    fn walk(mut answer: impl FnMut(u16) -> [u16; 2]) -> Option<(usize, ModelMap)> {
        let mut discovery = Discovery::new();
        let mut models = ModelMap::default();
        for step in 0..10_000 {
            let Some((address, _, purpose)) = discovery.next_step(&models) else {
                return Some((step, models));
            };
            match purpose {
                Purpose::Marker(base) => discovery.saw_marker(base, &MARKER, &mut models),
                _ => discovery.saw_header(address, &answer(address), &mut models),
            }
        }
        None
    }

    #[test]
    fn an_ordinary_chain_is_walked_to_its_sentinel() {
        // Two models and the end marker, at the lengths a real inverter
        // publishes.
        let models = walk(|address| match address {
            40_002 => [1, 66],
            40_070 => [103, 50],
            _ => [END_OF_CHAIN, 0],
        });
        let (_, found) = models.expect("it ends");
        assert!(found.has(1) && found.has(103));
    }

    #[test]
    fn a_device_that_answers_a_length_of_zero_does_not_walk_for_ever() {
        // The failure this bound exists for. Every step of the walk is driven by
        // two registers the *device* chose, and a header claiming a body of no
        // registers advances the address by two and never reaches the sentinel:
        // a box that reads one device for ever and never manages it, with
        // nothing saying why. A model is its body, so a header with none of one
        // ends the chain.
        let (steps, found) = walk(|_| [1, 0]).expect("it stops");
        assert!(steps < 8, "it stopped immediately, not after {steps} reads");
        assert!(!found.has(1), "and a body-less header is not a model");
    }

    #[test]
    fn a_device_that_never_sends_the_sentinel_stops_at_the_bound() {
        // The other shape: every header is plausible and the chain simply never
        // ends. Sixty-four models is far more than any real device publishes.
        let (steps, _) = walk(|_| [1, 4]).expect("it stops");
        assert!(
            steps <= usize::from(MAX_MODELS) + 4,
            "it followed {steps} headers"
        );
    }

    #[test]
    fn a_chain_that_would_run_off_the_end_of_the_address_space_stops() {
        // `40_000 + 2` and a length that overflows `u16`. Saturating here would
        // pin the address at the top and re-read it for ever.
        let (steps, _) = walk(|_| [1, u16::MAX - 40_000]).expect("it stops");
        assert!(steps < 8, "it stopped after {steps} reads");
    }
}
