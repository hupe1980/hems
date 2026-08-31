//! The same day, several weathers: the only evaluation that can judge a hedge.
//!
//! # Why a single day cannot
//!
//! A reference day runs **one** seeded realisation. That is enough to say what a
//! controller did and what it cost, and it is exactly the wrong instrument for a
//! plan that buys insurance: the premium is paid on every day and the claim is
//! made on one in ten. Measured on a realisation near the median, a hedge is a
//! pure loss — which is not a finding about the hedge, it is a property of
//! having looked once.
//!
//! It is the same failure the perfect-foresight bug had, wearing the other face.
//! There, a day whose forecast could not be wrong could not tell a good planner
//! from one shown the answer. Here, a day that happens once cannot tell a plan
//! that is *robust* from one that is merely lucky. Both are the harness deciding
//! the result before the controller does.
//!
//! So this runs the day under `n` seeds and reports the **distribution**: what
//! the household saves on an average day, what it saves on its worst, and how
//! far apart those are. A hedge that is worth buying shows up as a worse mean
//! and a better worst; one that is not shows up as a worse mean and nothing
//! else, and the number says so.
//!
//! # It is not a fleet back-test
//!
//! Every realisation comes from `hems-sim`'s own generator, so the days share
//! their statistics with the model that forecasts them (R23). What this measures
//! is the **difference between two controllers on the same set of days**, which
//! is a fair comparison and survives that objection; what it may not be used for
//! is an absolute saving figure shown to a customer.

use hems_forecast::Calibration;

use crate::scenario::{DayResult, Scenario, run};

/// One controller's outcome over a set of days.
#[derive(Debug, Clone, PartialEq)]
pub struct Spread {
    /// What the days saved, in the order they were run.
    pub savings_eur: Vec<f64>,
    /// What the days left the household short of the service it asked for, at
    /// the household's own prices — the thing a hedge exists to avoid.
    pub unserved_eur: Vec<f64>,
    /// How long the planning took, in seconds of wall clock, summed over the
    /// days. **Not** a benchmark: it is the number that says whether a policy
    /// fits inside a re-planning interval on a gateway box.
    pub seconds: f64,
    /// How the production band did across every day of the sweep.
    ///
    /// # Why it lives here and not on a day
    ///
    /// A day already scores its own forecasts, and a day's coverage figure is
    /// worth almost nothing: forecast error is correlated across a day, so
    /// ninety-six quarter hours of one Tuesday are close to **one** draw, and
    /// [`Calibration::is_well_calibrated`] refuses to answer on fewer than
    /// [`hems_forecast::CALIBRATION_DAYS`] of them.
    ///
    /// This sweep is the only thing in the workspace that produces independent
    /// days, and it was computing a `Calibration` per day and throwing it away.
    /// Merging them is what turns "the band looks wide on the June day" — a
    /// statement about one realisation — into a coverage figure that can be
    /// compared with the 80 % it promises. Run it with `--days 20` and
    /// `is_well_calibrated` becomes answerable rather than structurally false.
    pub pv_forecast: Calibration,
    /// The same for the load band.
    pub load_forecast: Calibration,
}

impl Spread {
    /// The mean saving.
    #[must_use]
    pub fn mean_saving_eur(&self) -> f64 {
        mean(&self.savings_eur)
    }

    /// The worst day's saving — the number a hedge is bought for.
    #[must_use]
    pub fn worst_saving_eur(&self) -> f64 {
        self.savings_eur
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
    }

    /// The best day's, so the spread can be read at a glance.
    #[must_use]
    pub fn best_saving_eur(&self) -> f64 {
        self.savings_eur
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// The mean cost of service the household asked for and did not get.
    #[must_use]
    pub fn mean_unserved_eur(&self) -> f64 {
        mean(&self.unserved_eur)
    }

    /// The worst day's.
    #[must_use]
    pub fn worst_unserved_eur(&self) -> f64 {
        self.unserved_eur
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// How many days it rests on.
    #[must_use]
    pub fn days(&self) -> usize {
        self.savings_eur.len()
    }

    /// Whether the sweep is long enough, and the bands wide enough and no
    /// wider, to call the forecast calibrated.
    ///
    /// Both bands, because a household plans against both and a planner handed
    /// one honest band and one overconfident one is not planning honestly.
    #[must_use]
    pub fn is_well_calibrated(&self) -> bool {
        self.pv_forecast.is_well_calibrated() && self.load_forecast.is_well_calibrated()
    }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// Run `scenario` under `seeds` different weathers and report the spread.
///
/// The seeds are derived from the scenario's own seed rather than drawn, so the
/// whole sweep is a pure function of the scenario and `days` — a back-test that
/// gave different answers on two runs would be the very thing D23 exists to
/// prevent, one level up.
///
/// # Errors
/// When the household described by the scenario is not a valid site.
pub fn spread_over_days(scenario: &Scenario, days: usize) -> anyhow::Result<Spread> {
    let mut savings = Vec::with_capacity(days);
    let mut unserved = Vec::with_capacity(days);
    let mut pv = Calibration::default();
    let mut load = Calibration::default();
    let started = std::time::Instant::now();
    for day in 0..days {
        let mut one = scenario.clone();
        // A different day with the same statistics. The multiplier is the same
        // golden-ratio odd constant `hems-sim` mixes with, so consecutive
        // indices give unrelated weathers rather than neighbouring ones.
        one.weather.seed = scenario
            .weather
            .seed
            .wrapping_add((day as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let result: DayResult = run(&one)?;
        savings.push(result.saving_eur());
        unserved.push(result.cost.unserved_eur);
        // Each day is one **episode**, whatever its slot count — which is the
        // distinction `Calibration` carries the two counts for.
        pv = pv.merge(result.pv_forecast);
        load = load.merge(result.load_forecast);
    }
    Ok(Spread {
        savings_eur: savings,
        unserved_eur: unserved,
        seconds: started.elapsed().as_secs_f64(),
        pv_forecast: pv,
        load_forecast: load,
    })
}
