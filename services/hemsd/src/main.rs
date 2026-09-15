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
        /// Small on purpose: three futures cost five to seven times the solve, so this
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
    /// Manage a real household: open the drivers' sockets and run the guard and
    /// the arbiter against them until the process is asked to stop.
    ///
    /// The other subcommands run a *simulated* day and finish in seconds. This
    /// one is the box on the wall.
    Run {
        /// The configuration file. Absent, or absent from disk, means the
        /// defaults — the reference household and no drivers at all, which is a
        /// box that keeps the house safe by assuming the worst about every
        /// device and says so at start-up.
        #[arg(long, env = "HEMS_HEMSD_CONFIG")]
        config: Option<PathBuf>,
        /// Check the configuration, build the site and the drivers, and exit
        /// without opening a socket.
        ///
        /// What an installer runs before leaving: every mismatch this daemon can
        /// refuse — a driver for an asset that does not exist, two drivers for
        /// one asset, a controllable device whose driver cannot command it, a
        /// § 14a household with nothing that could hear a reduction — is
        /// refused here rather than by a limit that never arrives.
        #[arg(long)]
        check: bool,
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
        /// Price carbon dioxide at this many euros per kilogram, so the plan
        /// prefers the hours the grid is clean and not only the hours it is
        /// cheap.
        ///
        /// Zero — the default — is the plain economic plan. 55 €/t, the German
        /// price for heating and transport fuels, is `0.055`. The intensity
        /// itself comes from the price stack, so what this buys is a plan that
        /// moves load towards clean hours and a figure for what that cost.
        #[arg(long)]
        co2_eur_per_kg: Option<f64>,
        /// Pay this many euros per kilowatt-hour to avoid taking one from the
        /// grid at all — the self-sufficiency dial.
        ///
        /// It is honest about its price: near the day's spread it makes the plan
        /// prefer its own roof even where importing would be marginally
        /// cheaper, and the difference is what independence cost. Somebody who
        /// bought a battery for autarky wants exactly that, and it belongs in
        /// the objective rather than in a marketing figure.
        #[arg(long)]
        autarky_eur_per_kwh: Option<f64>,
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
        /// signature over the report.
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
        /// The box's **own** two years. The house is never worse off when the
        /// cloud is gone, and a record that exists only once it has
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
            Risk::Expected => hems_optimizer::Risk::expected(),
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

/// Minutes of a local day as `HH:MM`, for the one line an installer reads back
/// against a price sheet.
fn clock(minutes: u16) -> String {
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60)
}

/// The box on the wall: configuration in, sockets open, guard and arbiter
/// running until somebody stops it.
///
/// # What it refuses to start with
///
/// Everything the registry can catch is caught before a byte moves (D75), and
/// `--check` is that half on its own — the command an installer runs before
/// leaving the cellar. The alternative is a box that comes up, looks healthy and
/// is never told about a reduction, which is the failure this daemon exists to
/// make impossible.
async fn manage(config: Option<&std::path::Path>, check: bool) -> anyhow::Result<()> {
    let settings: hemsd::Settings = hems_service::load(config, "HEMS_HEMSD")?;
    hems_service::init_tracing(
        hems_service::identity!(),
        &settings.service.log_filter,
        settings.service.log_json,
    );

    if check {
        let now = time::OffsetDateTime::now_utc();
        // No store: `run --check` validates the *configuration*, and reaching
        // into the box's record would make the answer depend on what an
        // operator happened to have written — which is exactly the thing the
        // installer is not checking here.
        let running = hemsd::runtime::assemble(&settings, None, now)?;
        // The SKI, printed rather than only logged: it is what an installer has
        // to give the metering point operator before a Steuerbox can be told to
        // trust this box, and it is the step field reports say goes wrong most
        // often. Deriving it here also *creates* it on a first run, so the
        // number does not change when the daemon starts for real.
        // The store is opened once for both credentials. Deriving them here
        // *creates* them on a first run, which is the point: an installer doing
        // a dry run carries away the same two numbers the daemon will serve
        // with, rather than two that change when it starts for real.
        let store = match &settings.store_path {
            Some(path) => Some(std::sync::Arc::new(tokio::sync::Mutex::new(
                hemsd::store::Store::open(path)?,
            ))),
            None => None,
        };
        if settings.ship.listen.is_some() {
            let (_, ski, _key) =
                hemsd::runtime::ship::identity(&settings.ship, store.as_ref(), now).await?;
            println!("🔑 SKI  {}", ski.to_display_string());
            println!("   give this to the metering point operator, so the Steuerbox trusts it");
        }
        // The other credential, and the one without which every surface below
        // answers `401`.
        let site = running.household.site.id.to_string();
        let issued = match &store {
            Some(store) => {
                let held = store.lock().await;
                hemsd::runtime::access::LocalAccess::resolve(&settings.api, &site, Some(&held))?.1
            }
            None => hemsd::runtime::access::LocalAccess::resolve(&settings.api, &site, None)?.1,
        };
        if let Some(announcement) = issued.announcement() {
            println!("{announcement}");
        }
        let now = time::OffsetDateTime::now_utc();
        let modes = std::collections::BTreeMap::new();
        let described = hems_flex::describe_site(
            &running.household.site,
            &hems_flex::DescribeContext::new(now, now + time::Duration::hours(24), &modes),
        );
        println!(
            "✅ {} assets, {} drivers, {} resources described in S2{}",
            running.household.site.assets.len(),
            settings.drivers.len(),
            described.resources.len(),
            match described.undescribed.len() {
                0 => String::new(),
                n => format!(", {n} it cannot express"),
            }
        );
        // The one thing an installer transcribes by hand from a PDF, and
        // therefore the one most likely to be a typo. `assemble` has already
        // refused a calendar that breaks the Anwendungshilfe; this says which
        // one passed, and where it came from, so the check leaves a record of
        // the document the household will be billed against.
        if let Some(m) = &settings.tariff.modul3 {
            println!(
                "📅 Modul 3 `{}` for {} conforms — HT {}–{}, NT {}–{}, billed in {}",
                m.id,
                m.year,
                clock(m.hochtarif_minutes[0]),
                clock(m.hochtarif_minutes[1]),
                clock(m.niedertarif_minutes[0]),
                clock(m.niedertarif_minutes[1]),
                m.billed_quarters.join(", "),
            );
            if let Some(source) = &m.source {
                println!("   transcribed from {source}");
            }
        }
        return Ok(());
    }

    let (signal, trigger) = hems_service::Shutdown::channel();
    tokio::spawn(hems_service::shutdown::on_signal(trigger));

    let health = hems_service::Health::new();
    // Not ready until a driver has actually been heard from. A box that reported
    // itself ready while every device was silent would be reporting that the
    // *process* was up, which is what liveness is for.
    health.bad("drivers", "no driver has reported yet");

    let running = hemsd::runtime::run(&settings, &health, &signal).await?;
    let site = running.household.site.id.to_string();
    // The second credential a commissioning visit carries away. The SKI goes to
    // the metering point operator so a Steuerbox trusts this box; this one goes
    // to whoever reads the box's own screens, and without it every surface below
    // answers `401`.
    if let Some(announcement) = &running.api_token {
        println!("{announcement}");
    }

    // The household's own surface, and — where the household has connected an
    // energy manager — the one that manager drives it over. Both on the socket
    // the shell already binds: one port to configure, one to firewall, and a
    // metrics label that is the route rather than every asset's name.
    let router = hemsd::runtime::surfaces(
        hemsd::runtime::api::Local::new(
            running.status,
            site,
            running.ski,
            running.overrides,
            running.trust,
            running.access.clone(),
            running.series,
        ),
        settings.s2.enabled.then(|| {
            hemsd::runtime::s2::Surface::new(
                std::sync::Arc::new(running.household.site.clone()),
                std::sync::Arc::clone(&running.registry),
                running.cem.clone(),
                settings.s2.clone(),
            )
        }),
        running.access.clone(),
    );

    hems_service::Server::new(
        hems_service::identity!(),
        settings.service.clone(),
        health,
        router,
    )
    .run_until(signal)
    .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = Cli::parse().command;
    // `run` configures its own logging from the file it is about to read, which
    // is the only subcommand with a file to read it from. The rest are
    // command-line tools and log for a person watching them.
    if !matches!(command, Command::Run { .. }) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "hemsd=info".into()),
            )
            .init();
    }

    match command {
        Command::Run { config, check } => return manage(config.as_deref(), check).await,
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
            co2_eur_per_kg,
            autarky_eur_per_kwh,
            report_to,
            report_secret,
            store,
            site,
            sharing,
        } => {
            let mut config = HouseholdConfig::default();
            if let (Some(wear), Some(battery)) = (wear_eur_per_kwh, &mut config.battery) {
                battery.wear_eur_per_kwh = wear;
            }
            if let Some(evse) = &mut config.evse {
                evse.switchable = !no_phase_switching;
            }
            if let Some(heat_pump) = &mut config.heat_pump {
                heat_pump.modulating = !heat_pump_on_off;
            }
            if imsys && let Some(pv) = &mut config.pv {
                // The network operator's first successful Ansteuerbarkeit test
                // — which is what § 9 Abs. 2 EEG actually waits for, and the
                // only thing this flag changes. The intelligent metering system
                // itself has been in since 2024 on both sides of the comparison,
                // so § 51's negative quarter hours are held constant and what
                // moves is the 60 % cap alone.
                pv.para9.relief = hems_core::prelude::CapRelief::ImsysWithControl;
            }
            let mut scenario = scenario_for(day, config);
            if perfect_foresight {
                scenario.weather = scenario.weather.with_perfect_forecast();
            }
            scenario.per_asset_weights = !uniform_weights;
            if let Some(price) = co2_eur_per_kg {
                scenario.objective = scenario.objective.with_carbon_price(price);
            }
            if let Some(premium) = autarky_eur_per_kwh {
                scenario.objective = scenario.objective.with_autarky_premium(premium);
            }
            if sharing {
                // Three roofs' worth of neighbours on the same street: the
                // household's own array times three, an equal third of the key.
                scenario.community = Some(hemsd::CommunityMembership::mehrfamilienhaus(
                    scenario
                        .config
                        .pv
                        .map_or(hems_core::prelude::Power::ZERO, |pv| pv.kwp)
                        * 3.0,
                ));
                // And on a day the rule reaches. § 42c Abs. 4 Nr. 1 obliges a
                // network operator to make sharing possible from 1 June 2026,
                // and every reference day is dated before that — so the same
                // day moves forward in whole weeks, keeping its weekday and its
                // season, rather than demonstrating an allocation nobody would
                // perform (D179).
                let before = scenario.date;
                scenario = scenario.on_a_day_sharing_reaches();
                if scenario.date != before {
                    println!(
                        "  § 42c reaches this household from {}, so the same day runs on {}",
                        hems_grid::sharing::SHARING_START,
                        scenario.date
                    );
                }
            }
            scenario.risk = risk.model();
            scenario.adaptive_risk = risk.adaptive();
            let result = hemsd::run(&scenario)?;

            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print!("{}", hemsd::render::day(&scenario, &result));
            }
            // Written **before** the report goes out. The order is the whole
            // point: the household's own record must not depend on a fleet
            // endpoint being up, and the day a network operator asks about is
            // exactly the day the link was down.
            if let Some(path) = &store {
                record_day(path, &result, time::OffsetDateTime::now_utc())?;
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
                    // `simulate` loads no configuration file, so the shell's
                    // defaults are the only trust decision available here: the
                    // platform's own store. A box on a wall reads its own
                    // `[service.http]` and may pin instead.
                    &hems_service::HttpSettings::default(),
                    secret.as_bytes(),
                    &site,
                    &result.kpis(&site, scenario.date),
                    store.as_deref(),
                    time::OffsetDateTime::now_utc(),
                )
                .await?;
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
    let recorded: Vec<hemsd::store::Recorded> = result
        .quarter_hours
        .iter()
        .map(|q| hemsd::store::Recorded {
            registers: *q,
            production: result.production_kwh_by_slot.get(&q.slot).copied(),
        })
        .collect();
    store.put_quarter_hours(&recorded, now)?;
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
/// A failure is **reported and not fatal** — a box whose fleet endpoint is down
/// has still managed its household correctly, and exiting non-zero would say
/// otherwise.
///
/// # A failed send is a queued day, where there is somewhere to queue it
///
/// With a `--store` the day is written to the outbox **before** the attempt, so
/// a fleet that is down costs a delay rather than a day; `hemsd run`'s drain
/// picks it up. Without one there is nowhere to put it, and the failure is the
/// end of that day — which is why `--store` is what a household runs with.
///
/// # Why the message id is the site and the day
///
/// `obsd` is idempotent by date: a box that comes back after an outage and
/// re-sends yesterday is correcting itself rather than adding a day. A random
/// identifier would make the *same* correction a different message every time,
/// so the identifier is derived from what the report is about, and the receiver
/// de-duplicates on the same string the signature covers.
async fn report_day(
    url: &str,
    http: &hems_service::HttpSettings,
    secret: &[u8],
    site: &str,
    kpis: &hems_core::report::DayKpis,
    store: Option<&std::path::Path>,
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
    let endpoint = format!("{}/v1/days", url.trim_end_matches('/'));
    // Checked before the day is built into a request, and it is the one failure
    // here that is **fatal**: a fleet endpoint that is down costs a dashboard,
    // and one that is plaintext costs the household's privacy every day until
    // somebody notices.
    hemsd::report::is_confidential(&endpoint)?;

    // Queued first. The order is the same one the evidence record uses and for
    // the same reason: what is written down survives the send failing, and what
    // is only in flight does not.
    let queued = match store {
        Some(path) => {
            let mut store = hemsd::store::Store::open(path)?;
            Some(store.queue_event(&event.id, hems_events::SITE_DAY_REPORTED, &body, now)?)
        }
        None => None,
    };

    let signature = hems_events::webhook::sign(secret, &event.id, now, &body);
    match hemsd::report::post_event(&endpoint, http, body, &signature.headers()).await {
        Ok(status) if (200..300).contains(&status) => {
            println!("\n  reported to {endpoint} — HTTP {status}");
            if let (Some(path), Some(id)) = (store, queued) {
                // Taken, so out of the backlog — by the row identifier the queue
                // handed back rather than by a search through it, which on a box
                // with a real backlog would not find the row it had just added.
                hemsd::store::Store::open(path)?.mark_sent(&[id], now)?;
            }
        }
        Ok(status) => {
            eprintln!("\n  {endpoint} answered HTTP {status}");
            eprintln!("  the day is queued and `hemsd run` will try again");
        }
        Err(e) => {
            eprintln!("\n  could not report to {endpoint}: {e}");
            if store.is_some() {
                eprintln!("  the day is queued and `hemsd run` will try again");
            }
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
