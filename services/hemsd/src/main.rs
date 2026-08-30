//! `hemsd` — the hems edge daemon.

use clap::{Parser, Subcommand};
use hemsd::{HouseholdConfig, Scenario};

#[derive(Parser)]
#[command(name = "hemsd", version, about = "The hems edge daemon", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
        /// Hand the planner the exact series the simulator is about to run.
        ///
        /// A comparison, never a default: the difference between this and an
        /// ordinary run is what forecast error costs a household. Any saving
        /// quoted from it is an upper bound no box in a real house can reach.
        #[arg(long)]
        perfect_foresight: bool,
        /// Give every asset the same allocation weight.
        ///
        /// Without per-asset shadow prices the guard's *weighted* max-min
        /// allocator is handed one number for the whole slot and weights
        /// nothing: a car three hours from its departure and a heat pump in a
        /// warm house get equal shares of a § 14a reduction.
        #[arg(long)]
        uniform_weights: bool,
    },
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
            perfect_foresight,
            uniform_weights,
        } => {
            let mut config = HouseholdConfig::default();
            if let Some(wear) = wear_eur_per_kwh {
                config.battery_wear_eur_per_kwh = wear;
            }
            config.evse_switchable = !no_phase_switching;
            if imsys {
                config.cap_relief = hems_core::prelude::CapRelief::ImsysWithControl;
            }
            let mut scenario = match day {
                Day::Winter => Scenario::winter_with_grid_event(config),
                Day::Summer => Scenario::summer_surplus(config),
                Day::Deadline => Scenario::winter_evening_deadline(config),
                Day::Shared => Scenario::winter_evening_no_store(&config),
                Day::Offline => Scenario::summer_without_a_planner(config),
                Day::Capped => Scenario::summer_capped(&config),
            };
            if perfect_foresight {
                scenario.weather = hemsd::WeatherSpec::PERFECT;
            }
            scenario.per_asset_weights = !uniform_weights;
            let result = hemsd::run(&scenario)?;

            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                print_report(&scenario, &result);
            }
        }
    }
    Ok(())
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
    if r.cold_water_kwh > 0.01 {
        row(
            "hot water not delivered",
            format!("{:.1} kWh", r.cold_water_kwh),
        );
    }
    if r.pv_forecast.samples > 0 {
        println!();
        row(
            "roof, as the box learned it",
            format!("{:.0} % of the model", r.roof_correction * 100.0),
        );
        row(
            "production forecast, CRPS",
            format!(
                "{:.0} W ({:.0} % covered)",
                r.pv_forecast.crps,
                r.pv_forecast.coverage * 100.0
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
    row("battery life spent", format!("{:.2} €", r.cost.wear_eur));
    row(
        "comfort given up",
        format!("{:.2} €", r.cost.discomfort_eur),
    );
    if r.cost.stored_eur > 0.005 {
        row(
            "borrowed from the stores",
            format!("{:.2} €", r.cost.stored_eur),
        );
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
    if r.unmet_charge_kwh > 0.01 {
        row(
            "car left short by",
            format!("{:.1} kWh", r.unmet_charge_kwh),
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
    row("described in S2", format!("{} resources", r.s2_resources));
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
