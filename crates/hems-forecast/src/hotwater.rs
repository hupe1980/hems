//! What a household draws out of its hot-water tank, and when.
//!
//! A four-person household uses six to seven kilowatt-hours of hot water a day,
//! and almost none of it at random: a shower before work, washing-up after
//! lunch, baths and dishes in the evening. That predictability is the whole
//! reason a tank is worth planning with — a store whose demand nobody can
//! forecast is a store nobody can pre-charge.
//!
//! # It is a prior, and it says so
//!
//! This is a fixed diurnal shape rather than something learned, and the reason
//! is the same one that keeps the building on `Rc2::house()`: nothing measures a
//! *draw*. MDT reports the tank's **temperature**, which is what the store's
//! state of charge is read from, and a draw would have to be inferred from a
//! temperature falling faster than the standing loss explains — which is a
//! different piece of work with its own failure modes (a cold fill and a shower
//! look alike for the first minute).
//!
//! So it is one shape, in one place, and the simulator and the running box plan
//! against the same one. That is not tidiness: a reference day whose planner saw
//! a different hot-water prior from the box's would be measuring a household
//! nobody can buy.

use hems_core::prelude::{Energy, Slot};

/// Heat drawn from the tank in one quarter hour.
///
/// Three peaks and a floor: a morning shower, washing-up after lunch, and the
/// evening — plus the trickle a household draws all day. Sampled at the middle
/// of the slot and taken over a quarter of an hour, which is the resolution the
/// planner works in.
#[must_use]
pub fn draw(slot: Slot) -> Energy {
    let hour = f64::from(slot.local_minute_of_day()) / 60.0;
    let peak = |centre: f64, width: f64, kwh: f64| kwh * (-((hour - centre) / width).powi(2)).exp();
    let kw = peak(7.0, 0.8, 1.7) + peak(13.0, 0.7, 0.6) + peak(20.0, 1.1, 1.4) + 0.03;
    Energy::from_kwh(kw * 0.25)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::Horizon;
    use time::macros::datetime;

    /// A day's draw is a household's day, not a constant.
    ///
    /// The figure is what makes the tank worth planning with at all: a shape
    /// that was flat would make a store worth exactly its standing loss, and the
    /// planner would rightly never pre-heat it.
    #[test]
    fn a_day_of_hot_water_is_about_what_a_family_uses() {
        let horizon = Horizon::new(datetime!(2026-01-15 00:00:00 UTC), 96);
        let total: f64 = horizon.slots().map(|s| draw(s).kwh()).sum();
        assert!(
            (5.0..8.0).contains(&total),
            "six to seven kilowatt-hours a day, got {total}"
        );
    }

    /// And it is concentrated, which is what a pre-charge is worth.
    #[test]
    fn the_morning_shower_is_the_shape_of_the_day() {
        let horizon = Horizon::new(datetime!(2026-01-15 00:00:00 UTC), 96);
        let at = |hour: usize| draw(horizon.slots().nth(hour * 4).expect("a slot")).kwh();
        assert!(
            at(7) > at(3) * 10.0,
            "seven in the morning is not three in the morning: {} against {}",
            at(7),
            at(3)
        );
    }
}
