//! Register blocks, as a household understands them.
//!
//! Every function here is a fold from one model's registers into the
//! [`Measurement`] the guard reads. The parsing itself is [`sunspec`]'s — the
//! register maps are generated from the specification and this crate does not
//! own a second copy of them.
//!
//! # Scale factors
//!
//! SunSpec stores a value and a separate signed exponent: `W = 231`,
//! `W_SF = 1` is 2 310 watts. Applying it is not optional and it is the single
//! most common way a SunSpec integration is wrong by a factor of ten — which is
//! a number that still looks like a plausible household.

use hems_core::prelude::{Energy, Measurement, Power, Soc};
use sunspec::Model;
use sunspec::models::model103::Model103;
use sunspec::models::model203::Model203;
use sunspec::models::model701::Model701;
use sunspec::models::model802::Model802;

/// The range SunSpec defines a scale factor over.
///
/// The specification puts it at −10 to 10, and nothing outside that is a scale
/// factor: it is a register that has been misread, a point the device does not
/// implement left at `0x8000`, or a body parsed at the wrong offset.
const SCALE_FACTOR_RANGE: std::ops::RangeInclusive<i16> = -10..=10;

/// Apply a SunSpec scale factor.
///
/// # It clamps, and a driver that did not would take the box down
///
/// `10^exponent` with an exponent read straight off a wire is an **infinity**
/// waiting to happen: one misaligned register turns a plausible 4 998 into
/// `10^4998`, and the `Power` it is handed to rejects a value that is not
/// finite. A panic in a driver is a panic in the control loop, and the input
/// that causes it comes from a device nobody in this workspace controls.
///
/// So an exponent outside the specification's own range is treated as **no
/// scaling** rather than as a number. That is not a guess about what the device
/// meant — it is a refusal to invent one, and it keeps the failure to a reading
/// that is wrong by a known power of ten instead of a box that stops managing
/// the house.
fn scaled(value: i64, exponent: i16) -> f64 {
    let exponent = if SCALE_FACTOR_RANGE.contains(&exponent) {
        exponent
    } else {
        0
    };
    let out = value as f64 * 10f64.powi(i32::from(exponent));
    if out.is_finite() { out } else { 0.0 }
}

/// Fold one model's registers into a measurement.
///
/// Unknown models are ignored rather than refused: a device publishes what it
/// publishes, and a driver that failed on an unfamiliar model would stop working
/// the day a manufacturer added one.
pub(crate) fn fold(id: u16, regs: &[u16], into: &mut Measurement) {
    match id {
        101..=103 => inverter(regs, into),
        201..=204 => meter(regs, into),
        802 => battery(regs, into),
        701 => der(regs, into),
        _ => {}
    }
}

/// Models 101–103: a photovoltaic inverter.
///
/// Reported in the **load convention** like everything else in this workspace:
/// an inverter produces, so its power is negative.
fn inverter(regs: &[u16], into: &mut Measurement) {
    let Ok(m) = Model103::parse(regs) else {
        return;
    };
    into.power = Some(Power::new(-scaled(i64::from(m.w), m.w_sf)));
    into.frequency_hz = Some(scaled(i64::from(m.hz), m.hz_sf));
    into.temperature_c = Some(scaled(i64::from(m.tmp_cab), m.tmp_sf));
    into.energy_out = Some(Energy::new(scaled(i64::from(m.wh), m.wh_sf)));
}

/// Models 201–204: a meter.
fn meter(regs: &[u16], into: &mut Measurement) {
    let Ok(m) = Model203::parse(regs) else {
        return;
    };
    into.power = Some(Power::new(scaled(i64::from(m.w), m.w_sf)));
    // A meter's frequency scale factor is optional in the model; an absent one
    // is an exponent of zero rather than a reason to drop the reading.
    into.frequency_hz = Some(scaled(i64::from(m.hz), m.hz_sf.unwrap_or(0)));
    into.energy_in = Some(Energy::new(scaled(i64::from(m.tot_wh_imp), m.tot_wh_sf)));
    into.energy_out = Some(Energy::new(scaled(i64::from(m.tot_wh_exp), m.tot_wh_sf)));
}

/// Model 802: a battery.
fn battery(regs: &[u16], into: &mut Measurement) {
    let Ok(m) = Model802::parse(regs) else {
        return;
    };
    into.soc = Soc::new(scaled(i64::from(m.soc), m.soc_sf) / 100.0).ok();
}

/// Model 701: DER measurement — the one that can say what a roof *could* do.
///
/// `ThrotPct` is how much throttling is in effect, so the unthrottled figure is
/// `W / (1 − ThrotPct)`. Without it a controller reads back exactly what it
/// commanded and never lifts its own curtailment.
///
/// A hundred per cent throttled is a real state (a roof commanded to zero) and
/// the division would be by zero, so it reports nothing rather than infinity:
/// "I cannot tell" is the honest answer there, and the caller's nameplate
/// fallback is better than a number that is not one.
fn der(regs: &[u16], into: &mut Measurement) {
    let Ok(m) = Model701::parse(regs) else {
        return;
    };
    let (Some(w), Some(sf)) = (m.w, m.w_sf) else {
        return;
    };
    let producing = -scaled(i64::from(w), sf);
    if into.power.is_none() {
        into.power = Some(Power::new(producing));
    }
    if let Some(throttled) = m.throt_pct {
        let fraction = f64::from(throttled) / 100.0;
        if fraction < 1.0 {
            into.available_power = Some(Power::new(producing.abs() / (1.0 - fraction)));
        }
    }
}
