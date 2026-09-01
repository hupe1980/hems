//! `hemsd` — the hems edge daemon.

use clap::{Parser, Subcommand};
use hemsd::{HouseholdConfig, Scenario};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "hemsd", version, about = "The hems edge daemon", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one day under several weathers, under each risk policy, and report
    /// what hedging costs and what it buys.
    ///
    /// The only evaluation that can judge a hedge: a single realisation pays the
    /// premium and never makes the claim, so it reports insurance as a pure
    /// loss. See `hemsd::backtest`.
    Risk {
        /// Which day to run.
        #[arg(long, value_enum, default_value_t = Day::Winter)]
        day: Day,
        /// How many weathers to run it under.
        ///
        /// Small on purpose: three futures cost seven times the solve, so this
        /// is minutes rather than seconds. Enough to see a sign, not enough to
        /// quote a figure.
        #[arg(long, default_value_t = 4)]
        days: usize,
        /// Report as JSON rather than a table.
        #[arg(long)]
        json: bool,
    },
    /// Run one day under many weathers and score the **forecast band** over
    /// them.
    ///
    /// The question `simulate` cannot answer. Forecast error is correlated
    /// across a day, so ninety-six quarter hours of one Tuesday are close to
    /// one draw: a day's coverage figure is a coin toss reported to three
    /// significant figures, and `Calibration::is_well_calibrated` refuses to
    /// answer on fewer than twenty independent days. This produces them.
    ///
    /// One policy — the ordinary median plan — because the box learns the same
    /// roof whatever it then does with the band, and running four policies to
    /// answer a question about the forecast costs four times as long for the
    /// same answer.
    Backtest {
        /// Which day to run.
        #[arg(long, value_enum, default_value_t = Day::Summer)]
        day: Day,
        /// How many weathers to run it under.
        #[arg(long, default_value_t = 20)]
        days: usize,
        /// Report as JSON rather than a table.
        #[arg(long)]
        json: bool,
    },
    /// Run one simulated day through the whole control stack and report on it.
    Simulate {
        /// Which day to run.
        #[arg(long, value_enum, default_value_t = Day::Winter)]
        day: Day,
        /// Report as JSON rather than a table.
        #[arg(long)]
        json: bool,
        /// Price battery wear at this many euros per kilowatt-hour of
        /// throughput. Zero reproduces a cost-only optimiser.
        #[arg(long)]
        wear_eur_per_kwh: Option<f64>,
        /// Wire the charge point to three fixed conductors, so it cannot drop to
        /// one when the surplus is too small for a three-phase session.
        #[arg(long)]
        no_phase_switching: bool,
        /// Run as though an intelligent metering system with a control device
        /// were in operation, which lifts the § 9 Abs. 2 EEG 60 % feed-in cap.
        #[arg(long)]
        imsys: bool,
        /// Fit a single-speed heat pump rather than a modulating one.
        ///
        /// The only configuration in which a minimum runtime constrains
        /// anything: the compressor is at its rating or off, so cycling is a
        /// decision the planner makes and the day can count. A modulating unit
        /// — what most German households have, and what every other reference
        /// day runs — has nothing to start.
        #[arg(long)]
        heat_pump_on_off: bool,
        /// Hand the planner the exact series the simulator is about to run.
        ///
        /// A comparison, never a default: the difference between this and an
        /// ordinary run is what forecast error costs a household. Any saving
        /// quoted from it is an upper bound no box in a real house can reach.
        #[arg(long)]
        perfect_foresight: bool,
        /// How the planner treats the fact that its forecasts are wrong.
        #[arg(long, value_enum, default_value_t = Risk::Median)]
        risk: Risk,
        /// Give every asset the same allocation weight.
        ///
        /// Without per-asset shadow prices the guard's *weighted* max-min
        /// allocator is handed one number for the whole slot and weights
        /// nothing: a car three hours from its departure and a heat pump in a
        /// warm house get equal shares of a § 14a reduction.
        #[arg(long)]
        uniform_weights: bool,
        /// Report the day to an `obsd` at this URL rather than only printing it.
        ///
        /// The fleet's own view of a household is a `DayKpis` — a dozen numbers
        /// rather than the whole report — and this is the producer of it. Without
        /// one, `obsd` is a service with no caller, which is the failure mode
        /// this workspace keeps finding in itself.
        #[arg(long, env = "HEMS_OBSD_URL")]
        report_to: Option<String>,
        /// The secret the box and the fleet share, for the Standard Webhooks
        /// signature over the report (D11).
        ///
        /// Required with `--report-to`, because a fleet view that accepts an
        /// unsigned day is a fleet view anybody who can reach it may write to —
        /// and what they would be writing is the list of households that did not
        /// respect a network operator's reduction.
        #[arg(long, env = "HEMS_OBSD_SECRET")]
        report_secret: Option<String>,
        /// Keep the day's § 14a evidence and quarter-hour registers in a local
        /// store at this path, `[A1 7.2]` and `[A1 7.3]`.
        ///
        /// The box's **own** two years. G3 says the house is never worse off
        /// when the cloud is gone, and a record that exists only once it has
        /// been uploaded is an intention with a network dependency — so the box
        /// records first and forwards second, and what has not been
        /// acknowledged is the store's outbox.
        #[arg(long, env = "HEMS_STORE")]
        store: Option<PathBuf>,
        /// The site identifier to report under.
        #[arg(long, default_value = "reference-household")]
        site: String,
        /// Put the household in a § 42c energy-sharing community: three roofs'
        /// worth of neighbours' array, an equal third of the
        /// Aufteilungsschlüssel, electricity at 12 ct/kWh net.
        ///
        /// § 42c EnWG has applied since 01.06.2026. The comparison is the point:
        /// the same day run with and without it says what a community is worth
        /// to a household that can *move* its load into the hours the community
        /// is generating — which is the whole behavioural reason to join one.
        #[arg(long)]
        sharing: bool,
    },
}

/// How the planner treats forecast error.
#[derive(Clone, Copy, clap::ValueEnum)]
enum Risk {
    /// One future: the median of both forecasts, priced as though it were
    /// certain. What every deterministic energy manager does.
    Median,
    /// Three futures from the band the forecast already carries, minimising the
    /// **expected** cost across them.
    Expected,
    /// The same three futures, with a third of the objective on the worst of
    /// them — a household that would rather not be caught out.
    Hedged,
    /// One median, and **three futures on the days that need them** — decided by
    /// how much slack the charging session has left.
    ///
    /// Over twenty seeded weathers on each of two days it comes within €0,04 of
    /// always planning against three futures on the evening that needs them and
    /// within €0,11 of the median on the day that does not — level with the
    /// median in money over the two, and delivering the service the median
    /// leaves short. Not the default, because it costs four times the solve to
    /// do it, which is a household's trade rather than an inherited one.
    Adaptive,
}

impl Risk {
    /// The starting policy, before any re-solve.
    fn model(self) -> hems_optimizer::Risk {
        match self {
            Risk::Median | Risk::Adaptive => hems_optimizer::Risk::deterministic(),
            Risk::Expected => hems_optimizer::Risk {
                cvar_weight: 0.0,
                ..hems_optimizer::Risk::hedged()
            },
            Risk::Hedged => hems_optimizer::Risk::hedged(),
        }
    }

    /// Whether a plan that says a service is at risk is re-solved against three
    /// futures.
    const fn adaptive(self) -> bool {
        matches!(self, Risk::Adaptive)
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Day {
    /// A January day with a § 14a reduction from 17:00 to 18:30.
    Winter,
    /// A June day with more production than the house can use, including four
    /// quarter hours of negative prices.
    Summer,
    /// A January evening with a § 14a reduction and a car that has to charge
    /// through it.
    Deadline,
    /// The same evening on a household with no store, where the reduction has
    /// to be shared and the allocation weights decide who gets it.
    Shared,
    /// The June day with the planner switched off — the box on its own.
    Offline,
    /// A September day with the planner off and broken cloud, where the surplus
    /// spends its time in the band only one conductor can use.
    Autumn,
    /// The June day with nobody home: the § 9 EEG 60 % cap against a roof the
    /// house cannot use.
    Capped,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hemsd=info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Simulate {
            day,
            json,
            wear_eur_per_kwh,
            no_phase_switching,
            imsys,
            heat_pump_on_off,
            perfect_foresight,
            risk,
            uniform_weights,
            report_to,
            report_secret,
            store,
            site,
            sharing,
        } => {
            let mut config = HouseholdConfig::default();
            if let Some(wear) = wear_eur_per_kwh {
                config.battery_wear_eur_per_kwh = wear;
            }
            config.evse_switchable = !no_phase_switching;
            config.heat_pump_modulating = !heat_pump_on_off;
            if imsys {
                // The network operator's first successful Ansteuerbarkeit test
                // — which is what § 9 Abs. 2 EEG actually waits for, and the
                // only thing this flag changes. The intelligent metering system
                // itself has been in since 2024 on both sides of the comparison,
                // so § 51's negative quarter hours are held constant and what
                // moves is the 60 % cap alone.
                config.para9.relief = hems_core::prelude::CapRelief::ImsysWithControl;
            }
            let mut scenario = scenario_for(day, config);
            if perfect_foresight {
                scenario.weather = hemsd::WeatherSpec::PERFECT;
            }
            scenario.per_asset_weights = !uniform_weights;
            if sharing {
                // Three roofs' worth of neighbours on the same street: the
                // household's own array times three, an equal third of the key.
                scenario.community = Some(hemsd::CommunityMembership::mehrfamilienhaus(
                    scenario.config.pv_kwp * 3.0,
                ));
            }
            scenario.risk = risk.model();
            scenario.adaptive_risk = risk.adaptive();
            let result = hemsd::run(&scenario)?;

            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_report(&scenario, &result);
            }
            // Written **before** the report goes out. The order is the whole
            // point: the household's own record must not depend on a fleet
            // endpoint being up, and the day a network operator asks about is
            // exactly the day the link was down.
            if let Some(path) = store {
                record_day(&path, &result, time::OffsetDateTime::now_utc())?;
            }
            if let Some(url) = report_to {
                let secret = report_secret.ok_or_else(|| {
                    anyhow::anyhow!(
                        "--report-to needs --report-secret (or HEMS_OBSD_SECRET): \
                         the fleet will not take an unsigned day"
                    )
                })?;
                report_day(
                    &url,
                    secret.as_bytes(),
                    &site,
                    &result.kpis(&site, scenario.date),
                    time::OffsetDateTime::now_utc(),
                )?;
            }
        }
        Command::Backtest { day, days, json } => {
            let mut scenario = scenario_for(day, HouseholdConfig::default());
            scenario.per_asset_weights = false;
            let spread = hemsd::spread_over_days(&scenario, days)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "day": scenario.date.to_string(),
                        "days": spread.days(),
                        "pv": {
                            "coverage": spread.pv_forecast.coverage,
                            "crps": spread.pv_forecast.crps,
                            "bias": spread.pv_forecast.bias,
                            "samples": spread.pv_forecast.samples,
                            "skipped": spread.pv_forecast.skipped,
                        },
                        "load": {
                            "coverage": spread.load_forecast.coverage,
                            "crps": spread.load_forecast.crps,
                            "bias": spread.load_forecast.bias,
                            "samples": spread.load_forecast.samples,
                            "skipped": spread.load_forecast.skipped,
                        },
                        "well_calibrated": spread.is_well_calibrated(),
                    }))?
                );
            } else {
                print_backtest(&scenario, &spread);
            }
        }
        Command::Risk { day, days, json } => {
            let config = HouseholdConfig::default();
            let base = scenario_for(day, config);
            let policies = [
                ("one median", Risk::Median),
                ("three futures", Risk::Expected),
                ("…and the tail", Risk::Hedged),
                ("only when at risk", Risk::Adaptive),
            ];
            let mut rows = Vec::new();
            for (label, policy) in policies {
                let mut scenario = base.clone();
                scenario.risk = policy.model();
                scenario.adaptive_risk = policy.adaptive();
                // The per-asset weights change no reference-day outcome and cost
                // a second solve of the same model; the sweep is about the
                // *plan*, so it runs without them and takes a third of the time.
                scenario.per_asset_weights = false;
                rows.push((label, hemsd::spread_over_days(&scenario, days)?));
            }
            if json {
                let as_json: Vec<_> = rows
                    .iter()
                    .map(|(label, s)| {
                        serde_json::json!({
                            "policy": label,
                            "days": s.days(),
                            "mean_saving_eur": s.mean_saving_eur(),
                            "worst_saving_eur": s.worst_saving_eur(),
                            "best_saving_eur": s.best_saving_eur(),
                            "mean_unserved_eur": s.mean_unserved_eur(),
                            "worst_unserved_eur": s.worst_unserved_eur(),
                            "seconds": s.seconds,
                            "pv_coverage": s.pv_forecast.coverage,
                            "pv_crps": s.pv_forecast.crps,
                            "load_coverage": s.load_forecast.coverage,
                            "load_crps": s.load_forecast.crps,
                            "well_calibrated": s.is_well_calibrated(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&as_json)?);
            } else {
                print_risk(&base, &rows);
            }
        }
    }
    Ok(())
}

/// Keep the day's evidence and registers in the box's own store.
///
/// Written **before** anything is sent anywhere, and nothing is marked forwarded
/// here: until a fleet client drains the outbox it grows, which is a number the
/// box can report about itself.
fn record_day(
    path: &std::path::Path,
    result: &hemsd::DayResult,
    now: time::OffsetDateTime,
) -> anyhow::Result<()> {
    let mut store = hemsd::store::Store::open(path)?;
    store.put_quarter_hours(&result.quarter_hours, now)?;
    for event in &result.evidence {
        store.put_control_event(event)?;
    }
    let backlog = store.backlog()?;
    println!(
        "\n  kept in {}\n  …waiting for the fleet      {} events, {} quarter hours",
        path.display(),
        backlog.events,
        backlog.quarter_hours
    );
    Ok(())
}

/// Send one day's KPIs to an `obsd`, as a signed CloudEvent.
///
/// A blocking POST from a command that has already finished: the day is over,
/// there is nothing to overlap it with, and a `tokio` runtime spun up to send
/// one request would be machinery for nothing. A failure is **reported and not
/// fatal** — a box whose fleet endpoint is down has still managed its household
/// correctly, and exiting non-zero would say otherwise.
///
/// # Why the message id is the site and the day
///
/// `obsd` is idempotent by date: a box that comes back after an outage and
/// re-sends yesterday is correcting itself rather than adding a day. A random
/// identifier would make the *same* correction a different message every time,
/// so the identifier is derived from what the report is about, and the receiver
/// de-duplicates on the same string the signature covers.
fn report_day(
    url: &str,
    secret: &[u8],
    site: &str,
    kpis: &hems_core::report::DayKpis,
    now: time::OffsetDateTime,
) -> anyhow::Result<()> {
    let event = hems_events::Event::new(
        hems_events::SITE_DAY_REPORTED,
        format!("hems://sites/{site}"),
        format!("{site}:{}", kpis.date),
        now,
        kpis,
    )
    .about(kpis.date.to_string());
    let body = event.to_bytes()?;
    let signature = hems_events::webhook::sign(secret, &event.id, now, &body);
    let endpoint = format!("{}/v1/days", url.trim_end_matches('/'));
    // Checked before the day is built into a request, and it is the one failure
    // here that is **fatal**: a fleet endpoint that is down costs a dashboard,
    // and one that is plaintext costs the household's privacy every day until
    // somebody notices.
    hemsd::report::is_confidential(&endpoint)?;
    match hemsd::report::post_event(&endpoint, body, &signature.headers()) {
        Ok(status) => {
            println!("\n  reported to {endpoint} — HTTP {status}");
        }
        Err(e) => {
            eprintln!("\n  could not report to {endpoint}: {e}");
        }
    }
    Ok(())
}

/// The scenario for one named day.
fn scenario_for(day: Day, config: HouseholdConfig) -> Scenario {
    match day {
        Day::Winter => Scenario::winter_with_grid_event(config),
        Day::Summer => Scenario::summer_surplus(config),
        Day::Deadline => Scenario::winter_evening_deadline(config),
        Day::Shared => Scenario::winter_evening_no_store(&config),
        Day::Offline => Scenario::summer_without_a_planner(config),
        Day::Autumn => Scenario::autumn_without_a_planner(config),
        Day::Capped => Scenario::summer_capped(&config),
    }
}

fn print_backtest(scenario: &Scenario, s: &hemsd::Spread) {
    println!(
        "\n  {} — {} weathers, the band scored against every one of them\n",
        scenario.date,
        s.days()
    );
    println!(
        "  {:<16}{:>10}{:>10}{:>9}{:>10}{:>9}",
        "band", "covered", "CRPS", "bias", "scored", "dark"
    );
    for (label, c) in [
        ("production", &s.pv_forecast),
        ("household load", &s.load_forecast),
    ] {
        println!(
            "  {:<16}{:>9.0}%{:>9.0}W{:>8.0}W{:>10}{:>9}",
            label,
            c.coverage * 100.0,
            c.crps,
            c.bias,
            c.samples,
            c.skipped
        );
    }
    println!(
        "\n  a 10–90 band should cover 80 %; `dark` is the quarter hours where\n  \
         there was nothing to forecast and which are therefore not scored.\n"
    );
    println!(
        "  {}\n",
        if s.is_well_calibrated() {
            "calibrated — over enough independent days to say so."
        } else if s.pv_forecast.episodes < hems_forecast::CALIBRATION_DAYS {
            "too few days to call it either way — try --days 20."
        } else {
            "not calibrated: the plan is hedging against a future of the wrong width."
        }
    );
}

fn print_risk(scenario: &Scenario, rows: &[(&str, hemsd::Spread)]) {
    let days = rows.first().map_or(0, |(_, s)| s.days());
    println!(
        "\n  {} — {days} weathers, the same household under each policy\n",
        scenario.date
    );
    println!(
        "  {:<16}{:>10}{:>10}{:>10}{:>12}{:>10}",
        "policy", "mean", "worst", "best", "unserved", "solve"
    );
    for (label, s) in rows {
        println!(
            "  {:<16}{:>9.2}€{:>9.2}€{:>9.2}€{:>11.2}€{:>9.0}s",
            label,
            s.mean_saving_eur(),
            s.worst_saving_eur(),
            s.best_saving_eur(),
            s.mean_unserved_eur(),
            s.seconds
        );
    }
    // The sweep is also the only thing in this workspace that produces
    // *independent* days, so it is the only thing that can say whether the band
    // the whole hedge is planned against is the width it claims to be. Read off
    // the first policy, because the forecasts do not depend on the policy — the
    // box learned the same roof either way.
    if let Some((_, s)) = rows.first() {
        let pv = &s.pv_forecast;
        let load = &s.load_forecast;
        println!(
            "\n  {:<16}{:>10}{:>10}{:>12}",
            "band", "covered", "CRPS", "episodes"
        );
        for (label, c, unit) in [("production", pv, "W"), ("household load", load, "W")] {
            println!(
                "  {:<16}{:>9.0}%{:>9.0}{unit}{:>12}",
                label,
                c.coverage * 100.0,
                c.crps,
                c.episodes
            );
        }
        println!(
            "\n  a 10–90 band should cover 80 %. {}",
            if s.is_well_calibrated() {
                "It does, over enough days to say so."
            } else if pv.episodes < hems_forecast::CALIBRATION_DAYS {
                "Too few days to say — run --days 20."
            } else {
                "It does not: the plan is hedging against a future that is the \
                 wrong width."
            }
        );
    }
    println!(
        "\n  mean is what the household saves on an average day; worst is the day\n  \
         it bought the insurance for. A hedge worth having has a lower mean and a\n  \
         higher worst — and if it has a lower mean and nothing else, it is not\n  \
         worth having, which is a result rather than a failure.\n"
    );
}

fn print_report(scenario: &Scenario, r: &hemsd::DayResult) {
    println!(
        "\n  {} — {}\n",
        scenario.date,
        if scenario.grid_event.is_some() {
            "with a § 14a reduction"
        } else {
            "no grid event"
        }
    );
    // A day the planner could not be surprised by is not a measurement of a
    // controller, and its saving is an upper bound rather than a result. Saying
    // so here is the whole of this project's argument applied to its own output:
    // the same winter day saves €2,09 honestly and €5,25 with the answer in
    // hand, and until this line existed both printed identically.
    if r.foresight_is_perfect {
        println!("  ⚠ the planner was shown the weather in advance — every figure");
        println!("    below is an upper bound, not a result\n");
    }
    let row = |label: &str, value: String| println!("  {label:<34} {value:>14}");
    row("produced", format!("{:.1} kWh", r.produced_kwh));
    row(
        "household consumption",
        format!("{:.1} kWh", r.consumed_kwh),
    );
    row(
        "charged into the car",
        format!("{:.1} kWh", r.ev_charged_kwh),
    );
    row("heat pump", format!("{:.1} kWh", r.heat_pump_kwh));
    row("hot water", format!("{:.1} kWh", r.dhw_kwh));
    if r.appliance_ran() {
        row(
            "dishwasher",
            format!(
                "{:.1} kWh, {} min later",
                r.appliance_kwh, r.appliance_shift_minutes
            ),
        );
    }
    row(
        "battery throughput",
        format!("{:.1} kWh", r.battery_throughput_kwh),
    );
    row("imported", format!("{:.1} kWh", r.imported_kwh));
    row("exported", format!("{:.1} kWh", r.exported_kwh));
    row("curtailed", format!("{:.1} kWh", r.curtailed_kwh));
    row(
        "peak feed-in, per quarter hour",
        match r.feed_in_ceiling_kw {
            Some(cap) => format!("{:.2} of {cap:.2} kW", r.peak_feed_in_kw),
            None => format!("{:.2} kW, uncapped", r.peak_feed_in_kw),
        },
    );
    row(
        "self-sufficiency",
        format!("{:.0} %", r.self_sufficiency * 100.0),
    );
    row(
        "wallbox on one conductor",
        format!(
            "{} min ({} switches)",
            r.single_phase_minutes, r.phase_switches
        ),
    );
    println!();
    row(
        "indoor temperature",
        format!("{:.1} – {:.1} °C", r.indoor_min_c, r.indoor_max_c),
    );
    row(
        "outside the comfort band",
        format!("{:.2} K·h", r.discomfort_kelvin_hours),
    );
    row(
        "hot-water tank, emptiest",
        format!("{:.0} % full", r.tank_min_fill * 100.0),
    );
    // Only where there is a compressor to cycle. A modulating unit has nothing
    // to start, and a structural zero printed every day is how a number stops
    // being read.
    if r.compressor_starts > 0 || r.compressor_held_minutes > 0 {
        row(
            "compressor starts",
            if r.compressor_held_minutes == 0 {
                format!("{}", r.compressor_starts)
            } else {
                // Time the unit's own minimum runtime overrode a command to
                // stop — the part of a plan the hardware does not carry out.
                format!(
                    "{} ({} min held against a command)",
                    r.compressor_starts, r.compressor_held_minutes
                )
            },
        );
    }
    if r.cold_water_kwh > 0.01 {
        row(
            "hot water not delivered",
            format!("{:.1} kWh", r.cold_water_kwh),
        );
    }
    // § 42c: only where there is a community, because a structural zero printed
    // every day is how a number stops being read.
    if scenario.community.is_some() {
        row(
            "allocated by the community",
            format!("{:.1} kWh", r.shared_kwh),
        );
    }
    if r.pv_forecast.samples > 0 {
        println!();
        row(
            "roof, as the box learned it",
            format!("{:.0} % of the model", r.roof_correction * 100.0),
        );
        // The slot count is on the line on purpose. A production score is over
        // the *lit* part of the day — a band of nothing against an outcome of
        // nothing is midnight, not a forecast that came true — and a reader who
        // cannot see how much of the day was scored cannot tell a good January
        // figure from an arithmetic about how long the night is.
        row(
            "production forecast, CRPS",
            format!(
                "{:.0} W ({:.0} % of {} lit)",
                r.pv_forecast.crps,
                r.pv_forecast.coverage * 100.0,
                r.pv_forecast.samples
            ),
        );
        row(
            "load forecast, CRPS",
            format!(
                "{:.0} W ({:.0} % covered)",
                r.load_forecast.crps,
                r.load_forecast.coverage * 100.0
            ),
        );
    }
    println!();
    row("electricity bill", format!("{:.2} €", r.cost.energy_eur));
    if r.cost.sharing_eur.abs() > 0.005 {
        row(
            "…less the community's own",
            format!("{:.2} €", r.cost.sharing_eur),
        );
    }
    row("battery life spent", format!("{:.2} €", r.cost.wear_eur));
    row(
        "comfort given up",
        format!("{:.2} €", r.cost.discomfort_eur),
    );
    if r.cost.curtailment_eur > 0.005 {
        row(
            "production thrown away",
            format!("{:.2} €", r.cost.curtailment_eur),
        );
    }
    if r.cost.unserved_eur > 0.005 {
        row(
            "service not delivered",
            format!("{:.2} €", r.cost.unserved_eur),
        );
    }
    if r.cost.stored_eur > 0.005 {
        row(
            "borrowed from the stores",
            format!("{:.2} €", r.cost.stored_eur),
        );
    }
    // Signed, and shown against the baseline's own entry: both households own
    // the same car, so the comparison is only fair once both are credited for
    // what is in it at midnight.
    let car = r.cost.vehicle_eur - r.baseline.vehicle_eur;
    if car.abs() > 0.005 {
        row("left in the car", format!("{car:+.2} €"));
    }
    row("cost of the day", format!("{:.2} €", r.cost.total()));
    row(
        "without optimisation",
        format!("{:.2} €", r.baseline.total()),
    );
    row("saved", format!("{:.2} €", r.saving_eur()));
    row(
        "…of it on the bill",
        format!("{:.2} €", r.bill_saving_eur()),
    );
    println!();
    row("§ 14a limit in force", format!("{} min", r.limited_minutes));
    if r.limited_minutes > 0 {
        row(
            "…against a minimum of",
            format!("{:.1} kW", r.minimum_power_kw),
        );
    }
    if r.commanded_below_minimum {
        row(
            "commanded below that minimum",
            "YES — unlawful, and recorded".to_string(),
        );
    }
    if r.failsafe_below_minimum {
        row(
            "own failsafe below that minimum",
            "YES — a configuration fault".to_string(),
        );
    }
    if r.lent_kwh > 0.005 {
        row("…covered by the store", format!("{:.1} kWh", r.lent_kwh));
    }
    row(
        "control events recorded",
        format!("{} ({} samples)", r.control_events, r.evidence_samples),
    );
    row("self-restraint records", format!("{}", r.failsafe_events));
    row(
        "slowest reaction",
        match r.acted_by_command {
            Some(true) => format!("{:.0} s, commanded", r.worst_latency_s),
            Some(false) => "0 s, already below".to_string(),
            None => format!("{:.0} s", r.worst_latency_s),
        },
    );
    row(
        "minutes without a plan",
        format!("{}", r.minutes_without_a_plan),
    );
    // What the plan that opened the day thought the day would cost, against what
    // it did. The seam between a forecast and a meter, in the currency everything
    // else in this report is in — and structurally zero for as long as the
    // planner was shown the answer, which is why it is worth printing.
    if let Some(expected) = r.opening_plan_bill_eur {
        row(
            "the opening plan expected",
            format!(
                "{expected:.2} €, off by {:+.2}",
                r.cost.billed_eur() - expected
            ),
        );
    }
    if r.unmet_charge_kwh > 0.01 {
        row(
            "car left short by",
            format!("{:.1} kWh", r.unmet_charge_kwh),
        );
    } else if r.planned_charge_shortfall_kwh > 0.01 {
        row(
            "a plan feared falling short by",
            format!("{:.1} kWh, and did not", r.planned_charge_shortfall_kwh),
        );
    }
    row(
        "without an Energy Guard",
        format!("{} min", r.failsafe_minutes),
    );
    row(
        "limit respected throughout",
        if r.grid_event_respected {
            "yes".to_string()
        } else {
            format!("NO, by {:.0} W", r.worst_overshoot_w)
        },
    );
    println!();
    if r.risk_re_solves > 0 {
        row(
            "re-solved against three futures",
            format!("{}×, because a service was at risk", r.risk_re_solves),
        );
    }
    row(
        "described in S2",
        match r.s2_undescribed {
            0 => format!("{} resources", r.s2_resources),
            n => format!("{} resources, {n} it cannot express", r.s2_resources),
        },
    );
    if r.widest_asset_value_ratio > 1.0 {
        row(
            "dearest asset vs cheapest",
            format!("{:.0}×", r.widest_asset_value_ratio),
        );
    }
    if r.relief_eur_per_kwh > 0.0 {
        row(
            "relief from § 14a was worth",
            format!("{:.2} €/kWh", r.relief_eur_per_kwh),
        );
    }
    if let Some(break_even) = r.modul2_break_even_kwh_per_year {
        row("Modul 2 pays above", format!("{break_even:.0} kWh/a"));
        row(
            "…on this day it would have",
            format!("{:+.2} € on the energy", r.modul2_delta_today_eur),
        );
    }
    println!();
}
