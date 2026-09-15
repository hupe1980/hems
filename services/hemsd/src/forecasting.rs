//! What the box believes about the day it is about to have.
//!
//! # Two series, deliberately different
//!
//! A simulated day whose forecast *is* the series the simulator is about to run
//! cannot tell a good planner from one that was shown the answer: every saving
//! it reports is an upper bound no box in a real house can reach, and the
//! arbiter's energy tracking — built precisely to absorb forecast error — is
//! never exercised, because the error is identically zero.
//!
//! So the simulator runs a [`Realisation`] and the planner is given what a box
//! could actually have known at midnight, which is **what it learned from the
//! weeks before**:
//!
//! | The planner is told | Where it comes from |
//! |---|---|
//! | production | the geometric model, corrected by [`ResidualModel`] for what this roof has actually been delivering, with the band its own dispersion earns |
//! | household load | [`LoadProfile`] — this household's own quarter hours, by day type, with empirical quantiles |
//! | the car | [`SessionHistory`] — when it usually comes home and how empty, until the cable actually goes in and it becomes a fact |
//! | outdoor temperature | the diurnal shape, without the day's own error |
//! | hot water | the household's usual draw, not this morning's |
//!
//! # The soiling the model does not know about
//!
//! The simulated roof delivers [`WeatherSpec::soiling`] of what its geometry
//! says — 92 % by default, which is an ordinary German roof with three years of
//! pollen, a little shading and modules that were never quite at their
//! datasheet. Nothing tells the model, and that is the point: the residual
//! corrector has to *find* it, exactly as it would in the field. A box that
//! believed the datasheet would size every morning's battery 8 % too small.
//!
//! # Still deterministic
//!
//! Everything here is a pure function of `(seed, instant)`, so a day replays to
//! the last cent. Determinism was never what had to go; being *told the answer*
//! was.

use hems_core::prelude::{Energy, GeoPoint, Horizon, Power, Slot};
use hems_forecast::{
    ArrayModel, Band, Calibration, Forecast, LoadProfile, ResidualModel, Session, SessionForecast,
    SessionHistory,
};
use hems_sim::Realisation;
use time::{Duration, OffsetDateTime};

/// How far the day that happens may stray from the day that was forecast.
///
/// Every amplitude at zero gives the planner perfect foresight, which is what
/// `--perfect-foresight` asks for: the comparison that shows what forecast error
/// costs. Having to name it is the point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeatherSpec {
    /// Which day this is. Same seed, same weather, for ever.
    pub seed: u64,
    /// How far the cloud cover strays from the day's mean.
    pub cloud_amplitude: f64,
    /// How far the household's behaviour strays from its profile.
    pub load_amplitude: f64,
    /// Half the diurnal temperature range, K. Part of the *forecast*: a day is
    /// colder before dawn than in the afternoon and a weather service says so.
    pub temperature_swing_k: f64,
    /// How far the temperature strays from the forecast shape, K.
    pub temperature_error_k: f64,
    /// How far a day's hot-water draw strays from the household's usual one.
    pub draw_amplitude: f64,
    /// What fraction of its modelled output the roof actually delivers.
    ///
    /// Soiling, shading, mismatch and module tolerance together. The forecast
    /// model does not know it; [`ResidualModel`] learns it.
    pub soiling: f64,
    /// Whether the planner is shown the weather the day will actually have.
    ///
    /// The **only** field that changes what the box knows rather than what the
    /// day is, and the reason it exists is that the other six cannot do this
    /// job. See [`WeatherSpec::with_perfect_forecast`].
    pub oracle_forecast: bool,
}

impl WeatherSpec {
    /// **This** day, with the planner shown the weather it will actually have.
    ///
    /// Changes nothing about the day — not the cloud, not the soiling, not the
    /// diurnal swing, not the household — and changes only what
    /// [`Weather::modelled_production`], [`Weather::forecast_outdoor_at`] and
    /// [`Weather::forecast_window_at`] answer. The unmanaged household is
    /// therefore **bit for bit the same** on both runs, which is the property
    /// that makes the difference attributable to foresight at all.
    ///
    /// # It used to change the day instead
    ///
    /// This was a `const` that set every amplitude to zero and the soiling to
    /// one, so `--perfect-foresight` ran a **different day** — a roof 8,7 %
    /// cleaner, a January night five kelvin milder, no cloud variability, an
    /// average household — and reported the difference as the price of
    /// imperfect knowledge (D197). Two of those are not forecast error under
    /// any reading: `temperature_swing_k` is in the forecast as well as in the
    /// realisation, so zeroing it removed no error and merely flattened the
    /// diurnal cycle; and `soiling` is a property of the roof that
    /// [`ResidualModel`] is built to learn, so after the warm-up it is not a
    /// forecast error on either side. The giveaway was the baseline: a
    /// household with no planner in it moved by €1,71 between the two runs, and
    /// nothing a *forecast* does can move a household that does not make one.
    ///
    /// # What it does not cover
    ///
    /// The **load** forecast, the hot-water draw and the charging session still
    /// come from what the box learned, so this is the price of not knowing the
    /// *weather* rather than of not knowing the future. That is the larger half
    /// and it is the half the name has always meant, but the report says
    /// "weather" now rather than "the future" because the two are not the same
    /// claim.
    #[must_use]
    pub const fn with_perfect_forecast(self) -> Self {
        Self {
            oracle_forecast: true,
            ..self
        }
    }

    /// A settled day: high pressure, a thin haze that comes and goes, an
    /// ordinary household.
    #[must_use]
    pub const fn settled(seed: u64) -> Self {
        Self {
            seed,
            cloud_amplitude: 0.18,
            load_amplitude: 0.25,
            temperature_swing_k: 5.0,
            temperature_error_k: 1.5,
            draw_amplitude: 0.35,
            soiling: 0.92,
            oracle_forecast: false,
        }
    }

    /// A day with weather in it: broken cloud, a front that is late.
    #[must_use]
    pub const fn broken(seed: u64) -> Self {
        Self {
            seed,
            cloud_amplitude: 0.35,
            load_amplitude: 0.3,
            temperature_swing_k: 4.0,
            temperature_error_k: 2.5,
            draw_amplitude: 0.4,
            soiling: 0.92,
            oracle_forecast: false,
        }
    }

    /// Whether the planner is being shown the answer.
    #[must_use]
    pub fn is_perfect(&self) -> bool {
        self.oracle_forecast
    }
}

/// The weather and behaviour a particular day actually has.
#[derive(Debug, Clone, Copy)]
pub struct Weather {
    /// How variable the day is.
    pub spec: WeatherSpec,
    /// The day's expected cloud cover, `0.0` clear.
    pub mean_cloud: f64,
    /// The day's expected mean outdoor temperature, °C.
    pub mean_outdoor_c: f64,
    /// This day's realisation.
    realisation: Realisation,
}

impl Weather {
    /// The weather of one day.
    #[must_use]
    pub fn new(spec: WeatherSpec, mean_cloud: f64, mean_outdoor_c: f64) -> Self {
        Self {
            spec,
            mean_cloud,
            mean_outdoor_c,
            realisation: Realisation::new(spec.seed),
        }
    }

    /// The same weather, `days` earlier — a different realisation with the same
    /// statistics, which is what the box's own history is made of.
    #[must_use]
    pub fn earlier(&self, days: u64) -> Self {
        Self {
            realisation: Realisation::new(self.spec.seed ^ (days.wrapping_mul(0x9E37_79B9) | 1)),
            ..*self
        }
    }

    /// The cloud cover that actually happens at an instant.
    #[must_use]
    pub fn cloud_at(&self, at: OffsetDateTime) -> f64 {
        self.realisation
            .cloud_cover(at, self.mean_cloud, self.spec.cloud_amplitude)
    }

    /// The outdoor temperature that actually happens.
    #[must_use]
    pub fn outdoor_at(&self, at: OffsetDateTime) -> f64 {
        self.realisation.outdoor_c(
            at,
            self.mean_outdoor_c,
            self.spec.temperature_swing_k,
            self.spec.temperature_error_k,
        )
    }

    /// The outdoor temperature the *forecast* says — the diurnal shape alone,
    /// or the day's own temperature where the planner is being shown the answer.
    #[must_use]
    pub fn forecast_outdoor_at(&self, at: OffsetDateTime) -> f64 {
        if self.spec.oracle_forecast {
            return self.outdoor_at(at);
        }
        Realisation::forecast_outdoor_c(at, self.mean_outdoor_c, self.spec.temperature_swing_k)
    }

    /// What the household actually draws, given its usual profile.
    #[must_use]
    pub fn load_at(&self, at: OffsetDateTime, usual: Power) -> Power {
        usual * self.realisation.load_factor(at, self.spec.load_amplitude)
    }

    /// What the household actually draws from the tank, given its usual draw.
    #[must_use]
    pub fn draw_in(&self, slot: Slot, usual: Energy) -> Energy {
        usual * self.realisation.draw_factor(slot, self.spec.draw_amplitude)
    }

    /// The irradiance that actually falls on the building's glazing, W/m².
    ///
    /// The realised cloud, on a vertical plane at `facade_azimuth_deg` — what
    /// [`hems_core::thermal::Rc2::free_heat_kw`] turns into the heat the house
    /// gets whether or not anything asked for it.
    ///
    /// It goes through the **global horizontal** value and is then transposed,
    /// which is not how [`Weather::production_at`] treats the roof: that one
    /// scales the clear-sky *power* by `1 − cloud`. The difference is not an
    /// inconsistency about the weather — both read the same realised cloud —
    /// but about what a cloud does to a plane, and a vertical one facing the
    /// low winter sun cannot be got at by scaling a horizontal quantity.
    #[must_use]
    pub fn window_at(
        &self,
        location: GeoPoint,
        at: OffsetDateTime,
        facade_azimuth_deg: f64,
    ) -> f64 {
        let slot = Slot::containing(at);
        let sun = hems_forecast::solar::sun_position(location, slot);
        let ghi = hems_forecast::clear_sky_ghi(sun) * (1.0 - self.cloud_at(at)).max(0.0);
        hems_forecast::solar::window_irradiance(sun, ghi, facade_azimuth_deg)
    }

    /// The same, as the *forecast* sees it — the mean cloud rather than this
    /// day's own.
    #[must_use]
    pub fn forecast_window_at(
        &self,
        location: GeoPoint,
        slot: Slot,
        facade_azimuth_deg: f64,
    ) -> f64 {
        if self.spec.oracle_forecast {
            return self.window_at(location, slot_middle(slot), facade_azimuth_deg);
        }
        let sun = hems_forecast::solar::sun_position(location, slot);
        let ghi = hems_forecast::clear_sky_ghi(sun) * (1.0 - self.mean_cloud).max(0.0);
        hems_forecast::solar::window_irradiance(sun, ghi, facade_azimuth_deg)
    }

    /// What the roof actually produces, as a positive magnitude in watts.
    ///
    /// The realised cloud, the realised temperature *and* the soiling the
    /// forecast model has never been told about.
    #[must_use]
    pub fn production_at(
        &self,
        array: &ArrayModel,
        location: GeoPoint,
        at: OffsetDateTime,
    ) -> Power {
        let slot = Slot::containing(at);
        let clear = array
            .clear_sky_power(location, slot, self.outdoor_at(at))
            .outflow();
        clear * ((1.0 - self.cloud_at(at)) * self.spec.soiling)
    }

    /// What the geometric model *says* the roof will produce, as a positive
    /// magnitude — the input the residual corrector corrects.
    #[must_use]
    pub fn modelled_production(&self, array: &ArrayModel, location: GeoPoint, slot: Slot) -> Power {
        if self.spec.oracle_forecast {
            // The soiling included: an oracle is not merely told the cloud, it
            // is told what the roof will make — which is what leaves
            // `ResidualModel` with a correction of exactly one and nothing to
            // learn, and is the whole point of the comparison.
            return self.production_at(array, location, slot_middle(slot));
        }
        let ambient = self.forecast_outdoor_at(slot.start());
        array.clear_sky_power(location, slot, ambient).outflow() * (1.0 - self.mean_cloud)
    }
}

/// The middle of a slot — where a quarter hour is sampled when one instant has
/// to stand for it, the same convention [`hems_forecast::solar::sun_position`]
/// uses and for the same reason.
fn slot_middle(slot: Slot) -> OffsetDateTime {
    slot.start() + hems_core::slot::SLOT / 2_i32
}

/// What the box has learned from the days before this one.
#[derive(Debug, Clone)]
pub struct Learned {
    /// This household's own load profile, by day type and quarter hour.
    pub load: LoadProfile,
    /// What this roof delivers against what its geometry says.
    pub roof: ResidualModel,
    /// When the car usually comes home, and how empty.
    pub sessions: SessionHistory,
    /// Whether the roof is still the roof it was.
    ///
    /// Beside [`Learned::roof`] rather than inside it, because the two ask
    /// opposite questions of the same numbers: the corrector has to **follow**
    /// the array so the plan stays good, and this has to **notice it moving** so
    /// the household finds out (D199). It is here rather than only on the
    /// running box because a module the reference days never reach is a module
    /// whose KPI cannot move, which is R20 exactly — and D199's own monitor was
    /// built, wired into `hemsd run`, and never given a simulated day.
    pub health: hems_forecast::PlantHealth,
    /// How many days of history it rests on.
    pub days: usize,
}

/// How long a box watches before it is worth calling what it has a forecast.
///
/// Six weeks. A cell of [`LoadProfile`] is one quarter hour of one day *type*,
/// so a Sunday cell gains one observation a week: three weeks gives it the three
/// samples `MIN_SAMPLES` asks for, and three samples produce a band the outcome
/// falls inside two times in five. Six weeks gives every day type six, which
/// with the small-sample widening in [`LoadProfile::band_at`] is a band worth
/// planning against.
///
/// It costs nothing to simulate — the warm-up meters and does not decide, so no
/// solver runs — and it is about how long a household waits before it starts
/// asking why the box has not saved anything yet.
pub const WARM_UP_DAYS: usize = 42;

/// Watch the household for [`WARM_UP_DAYS`] days without controlling anything.
///
/// This is the box's first three weeks: it meters, it does not decide. The
/// simulator's own weather generator produces those days, so the history is
/// consistent with the day that follows without being *the same as* it.
///
/// `usual_load` is the household's underlying profile — the thing the realisation
/// perturbs, and the thing the box cannot see directly.
///
/// There is deliberately **no hot-water draw here**. The tank's usual draw is a
/// fixed prior shared by the box and the days (`hems_forecast::hotwater::draw`),
/// so a warm-up has nothing about it to learn. The day it becomes something the
/// box learns, it belongs on `Learned` rather than in a closure passed through
/// here.
///
/// `days` is how long it watched. **Zero is a box on its first evening** — no
/// profile, no correction, no session history — which is hour one of every real
/// installation and is a case worth running rather than assuming (D187).
pub fn warm_up(
    days: usize,
    weather: &Weather,
    array: &ArrayModel,
    location: GeoPoint,
    start: OffsetDateTime,
    usual_load: impl Fn(Slot) -> Power,
    session: Option<(Duration, Duration, Energy)>,
) -> Learned {
    let mut learned = Learned {
        load: LoadProfile::default(),
        roof: ResidualModel::default(),
        sessions: SessionHistory::new(),
        health: hems_forecast::PlantHealth::new(),
        days,
    };

    for day in 1..=days {
        let past = weather.earlier(day as u64);
        let midnight = start - Duration::days(i64::try_from(day).unwrap_or(0));
        for k in 0..96 {
            let slot = Slot::containing(midnight + Duration::minutes(k * 15));
            // The roof: what the model said against what the meter saw. The
            // corrector is fed the *slot's* energy rather than an instant, which
            // is what a meter reports and what a quarter-hour plan consumes.
            let modelled = past.modelled_production(array, location, slot);
            let actual = past.production_at(array, location, slot.start() + Duration::minutes(7));
            learned.roof.observe(slot, modelled.get(), actual.get());
            // The same two numbers to the monitor watching for the array to
            // move, as watts over a quarter hour turned into kilowatt-hours.
            learned.health.observe_slot(
                modelled.get() * hems_core::prelude::SLOT_HOURS / 1000.0,
                actual.get() * hems_core::prelude::SLOT_HOURS / 1000.0,
            );
            // The household: what it drew.
            learned
                .load
                .observe(slot, past.load_at(slot.start(), usual_load(slot)));
        }
        // The car, on the weekdays it comes home. The jitter is the same
        // realisation, so a household with a regular life gets a tight forecast
        // and one without gets a wide one.
        if let Some((arrival, departure, energy)) = session {
            let jitter = past.realisation.load_factor(midnight + arrival, 0.12) - 1.0;
            let plugged_in = midnight + arrival + Duration::seconds_f64(jitter * 5400.0);
            let unplugged = midnight + departure + Duration::seconds_f64(jitter * 1800.0);
            learned.sessions.observe(Session {
                plugged_in,
                unplugged: unplugged.max(plugged_in + Duration::hours(1)),
                energy: energy * (1.0 + jitter),
            });
        }
        // The roof's day, closed. A performance ratio is a daily figure and the
        // monitor is fed one at a time, so this is where a warm-up day becomes
        // an observation rather than ninety-six of them.
        //
        // The days are **exchangeable** rather than chronological —
        // `Weather::earlier` reseeds the realisation rather than stepping back
        // through a season — so the order they arrive in decides nothing, and a
        // trend is something no warm-up can contain. That is the honest limit of
        // what a simulated baseline says about a real roof (R23).
        let _ = learned.health.close_day();
    }
    learned
}

/// The photovoltaic forecast for a horizon: the geometric model, corrected.
#[must_use]
pub fn pv_forecast(
    learned: &Learned,
    weather: &Weather,
    array: &ArrayModel,
    location: GeoPoint,
    horizon: Horizon,
) -> Forecast {
    Forecast {
        slots: horizon
            .slots()
            .map(|slot| {
                let modelled = weather.modelled_production(array, location, slot);
                (slot, learned.roof.correct(slot, modelled.get()))
            })
            .collect(),
    }
}

/// The load forecast a box plans against — warm or **cold**.
///
/// One function because there are two callers and they disagreed. The running
/// box gated on `LoadProfile::is_empty` and fell back to persistence from its
/// own meter; the reference days called `forecast` unconditionally. A profile
/// with no history at all answers `p10 = p50 = p90 = 0` — *the house will use
/// nothing, and I am sure* — so a simulated cold box planned against a household
/// that does not exist, deferred every flexible kilowatt-hour into hours it
/// believed were free, and came out **worse than having no energy manager**.
/// That was a property of the harness rather than of the product, which is the
/// worst kind of difference to have: it measures a path production does not run
/// (D187).
///
/// It takes the **profile** rather than either of this daemon's two `Learned`
/// types, because the decision is about the profile: a caller that has one can
/// ask, whichever bundle it keeps it in.
///
/// `measured_now` is what the household's own meter says at the start of the
/// horizon. `None` where nothing is measuring it, and then there is no forecast
/// to be had: a box that cannot read its own connection point has no load to
/// persist and inventing one would plan a house nobody is watching.
#[must_use]
pub fn load_forecast(
    profile: &LoadProfile,
    horizon: Horizon,
    measured_now: Option<Power>,
) -> Option<Forecast> {
    if !profile.is_empty() {
        return Some(profile.forecast(horizon));
    }
    // Doubling by this time tomorrow, which is about what a single reading is
    // worth twenty-four hours out.
    measured_now.map(|recent| hems_forecast::naive::persistence(recent, horizon, 0.9))
}

/// The charging session the planner should be given at `now`.
///
/// Two regimes, and confusing them is what made the old `deadline` scenario
/// demonstrate nothing:
///
/// * **before the cable goes in** the planner gets the *forecast* session, or
///   nothing at all where the history does not support one. A plan that reserves
///   the cheap hours for a car the household has not committed to is a guess,
///   and it is one the household pays for if the car does not come.
/// * **from the moment it is plugged in** the session is a fact — the vehicle
///   reports its charge, the household says when it needs the car — and the
///   forecast is not consulted again.
#[must_use]
pub fn session_at(
    learned: &Learned,
    now: OffsetDateTime,
    midnight: OffsetDateTime,
    actual_arrival: OffsetDateTime,
) -> Option<SessionForecast> {
    if now >= actual_arrival {
        return None;
    }
    learned
        .sessions
        .forecast_for(Slot::containing(midnight).local_date(), midnight)
}

/// How a forecast did, scored against what happened.
///
/// The one number that says whether a saving figure means anything: a day whose
/// forecasts scored a CRPS of zero is a day with perfect foresight, and its
/// saving is an upper bound rather than a result.
#[must_use]
pub fn score(pairs: &[(Band, f64)]) -> Calibration {
    Calibration::score(pairs.iter().copied())
}
