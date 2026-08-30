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
}

impl WeatherSpec {
    /// A day that goes exactly as forecast.
    ///
    /// Nameable rather than implicit, because the difference between this and a
    /// real day is the most interesting single number the simulator produces.
    pub const PERFECT: Self = Self {
        seed: 0,
        cloud_amplitude: 0.0,
        load_amplitude: 0.0,
        temperature_swing_k: 0.0,
        temperature_error_k: 0.0,
        draw_amplitude: 0.0,
        soiling: 1.0,
    };

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
        }
    }

    /// Whether this day is the degenerate one in which nothing can be wrong.
    #[must_use]
    pub fn is_perfect(&self) -> bool {
        self.cloud_amplitude == 0.0
            && self.load_amplitude == 0.0
            && self.temperature_error_k == 0.0
            && self.draw_amplitude == 0.0
            && (self.soiling - 1.0).abs() < f64::EPSILON
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

    /// The outdoor temperature the *forecast* says — the diurnal shape alone.
    #[must_use]
    pub fn forecast_outdoor_at(&self, at: OffsetDateTime) -> f64 {
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
        let ambient = self.forecast_outdoor_at(slot.start());
        array.clear_sky_power(location, slot, ambient).outflow() * (1.0 - self.mean_cloud)
    }
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
/// `usual_load` and `usual_draw` are the household's underlying profiles — the
/// thing the realisation perturbs, and the thing the box cannot see directly.
pub fn warm_up(
    weather: &Weather,
    array: &ArrayModel,
    location: GeoPoint,
    start: OffsetDateTime,
    usual_load: impl Fn(Slot) -> Power,
    usual_draw: impl Fn(Slot) -> Energy,
    session: Option<(Duration, Duration, Energy)>,
) -> Learned {
    let mut learned = Learned {
        load: LoadProfile::default(),
        roof: ResidualModel::default(),
        sessions: SessionHistory::new(),
        days: WARM_UP_DAYS,
    };
    let _ = usual_draw;

    for day in 1..=WARM_UP_DAYS {
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

/// The load forecast for a horizon, from the household's own history.
#[must_use]
pub fn load_forecast(learned: &Learned, horizon: Horizon) -> Forecast {
    learned.load.forecast(horizon)
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
