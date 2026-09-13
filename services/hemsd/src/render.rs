//! What a simulated day prints.
//!
//! A `String` rather than a series of `println!`s, because the report is a
//! **product surface**: it is what `hemsd simulate` shows, what the landing page
//! reproduces, and what a reader compares against the money. A surface that
//! exists only inside `main` is one no test can reach, and a transcript copied
//! anywhere else then goes stale in silence.

use crate::{DayResult, Scenario};

/// The report `hemsd simulate` prints for one day.
///
/// Every line is a label and a value, two spaces or more apart, which is what
/// lets a reader line the money up against the energies and what lets a test
/// hold the landing page to it.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn day(scenario: &Scenario, r: &DayResult) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
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
        let _ = writeln!(
            out,
            "  ⚠ the planner was shown the weather in advance — every figure"
        );
        let _ = writeln!(out, "    below is an upper bound, not a result\n");
    }
    let row = |out: &mut String, label: &str, value: String| {
        let _ = writeln!(out, "  {label:<34} {value:>14}");
    };
    row(&mut out, "produced", format!("{:.1} kWh", r.produced_kwh));
    row(
        &mut out,
        "household consumption",
        format!("{:.1} kWh", r.consumed_kwh),
    );
    row(
        &mut out,
        "charged into the car",
        format!("{:.1} kWh", r.ev_charged_kwh),
    );
    row(&mut out, "heat pump", format!("{:.1} kWh", r.heat_pump_kwh));
    row(&mut out, "hot water", format!("{:.1} kWh", r.dhw_kwh));
    if r.appliance_ran() {
        row(
            &mut out,
            "dishwasher",
            format!(
                "{:.1} kWh, {} min later",
                r.appliance_kwh, r.appliance_shift_minutes
            ),
        );
    }
    row(
        &mut out,
        "battery throughput",
        format!("{:.1} kWh", r.battery_throughput_kwh),
    );
    row(&mut out, "imported", format!("{:.1} kWh", r.imported_kwh));
    row(&mut out, "exported", format!("{:.1} kWh", r.exported_kwh));
    row(&mut out, "curtailed", format!("{:.1} kWh", r.curtailed_kwh));
    row(
        &mut out,
        "peak feed-in, per quarter hour",
        match r.feed_in_ceiling_kw {
            Some(cap) => format!("{:.2} of {cap:.2} kW", r.peak_feed_in_kw),
            None => format!("{:.2} kW, uncapped", r.peak_feed_in_kw),
        },
    );
    row(
        &mut out,
        "self-sufficiency",
        format!("{:.0} %", r.self_sufficiency * 100.0),
    );
    row(
        &mut out,
        "wallbox on one conductor",
        format!(
            "{} min ({} switches)",
            r.single_phase_minutes, r.phase_switches
        ),
    );
    let _ = writeln!(out);
    row(
        &mut out,
        "indoor temperature",
        format!("{:.1} – {:.1} °C", r.indoor_min_c, r.indoor_max_c),
    );
    row(
        &mut out,
        "outside the comfort band",
        format!("{:.2} K·h", r.discomfort_kelvin_hours),
    );
    row(
        &mut out,
        "hot-water tank, emptiest",
        format!("{:.0} % full", r.tank_min_fill * 100.0),
    );
    // Only where there is a compressor to cycle. A modulating unit has nothing
    // to start, and a structural zero printed every day is how a number stops
    // being read.
    if r.compressor_starts > 0 || r.compressor_held_minutes > 0 {
        row(
            &mut out,
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
            &mut out,
            "hot water not delivered",
            format!("{:.1} kWh", r.cold_water_kwh),
        );
    }
    // § 42c: only where there is a community, because a structural zero printed
    // every day is how a number stops being read.
    if scenario.community.is_some() {
        row(
            &mut out,
            "allocated by the community",
            format!("{:.1} kWh", r.shared_kwh),
        );
    }
    if r.pv_forecast.samples > 0 {
        let _ = writeln!(out);
        row(
            &mut out,
            "roof, as the box learned it",
            format!("{:.0} % of the model", r.roof_correction * 100.0),
        );
        // The slot count is on the line on purpose. A production score is over
        // the *lit* part of the day — a band of nothing against an outcome of
        // nothing is midnight, not a forecast that came true — and a reader who
        // cannot see how much of the day was scored cannot tell a good January
        // figure from an arithmetic about how long the night is.
        row(
            &mut out,
            "production forecast, CRPS",
            format!(
                "{:.0} W ({:.0} % of {} lit)",
                r.pv_forecast.crps,
                r.pv_forecast.coverage * 100.0,
                r.pv_forecast.samples
            ),
        );
        row(
            &mut out,
            "load forecast, CRPS",
            format!(
                "{:.0} W ({:.0} % covered)",
                r.load_forecast.crps,
                r.load_forecast.coverage * 100.0
            ),
        );
    }
    let _ = writeln!(out);
    row(
        &mut out,
        "electricity bill",
        format!("{:.2} €", r.cost.energy_eur),
    );
    // What the same day costs if the quarter hour is netted before it is priced
    // — the way a meter does not work and most published comparisons do. Shown
    // only when it differs by a cent, because on a day with no quarter hour
    // carrying both directions there is nothing to say.
    // What the same day costs if the quarter hour is netted before it is priced
    // — the way a meter does not work and most published comparisons do. What is
    // printed is its effect on the **saving**, because netting does not forgive
    // the two households equally and a difference is where that shows.
    let netted_saving = (r.baseline_energy_eur_netted - r.energy_eur_netted)
        - (r.baseline.energy_eur - r.cost.energy_eur);
    if netted_saving.abs() >= 0.005 {
        row(
            &mut out,
            "…if the quarter hour were netted",
            format!(
                "{netted_saving:+.2} € on the bill saving, which a two-register meter does not forgive"
            ),
        );
    }
    if r.cost.sharing_eur.abs() > 0.005 {
        row(
            &mut out,
            "…less the community's own",
            format!("{:.2} €", r.cost.sharing_eur),
        );
    }
    row(
        &mut out,
        "battery life spent",
        format!("{:.2} €", r.cost.wear_eur),
    );
    row(
        &mut out,
        "comfort given up",
        format!("{:.2} €", r.cost.discomfort_eur),
    );
    if r.cost.curtailment_eur > 0.005 {
        row(
            &mut out,
            "production thrown away",
            format!("{:.2} €", r.cost.curtailment_eur),
        );
    }
    if r.cost.unserved_eur > 0.005 {
        row(
            &mut out,
            "service not delivered",
            format!("{:.2} €", r.cost.unserved_eur),
        );
    }
    if r.cost.stored_eur > 0.005 {
        row(
            &mut out,
            "borrowed from the stores",
            format!("{:.2} €", r.cost.stored_eur),
        );
    }
    // Signed, and shown against the baseline's own entry: both households own
    // the same car, so the comparison is only fair once both are credited for
    // what is in it at midnight.
    let car = r.cost.vehicle_eur - r.baseline.vehicle_eur;
    if car.abs() > 0.005 {
        row(&mut out, "left in the car", format!("{car:+.2} €"));
    }
    row(
        &mut out,
        "cost of the day",
        format!("{:.2} €", r.cost.total()),
    );
    row(
        &mut out,
        "without optimisation",
        format!("{:.2} €", r.baseline.total()),
    );
    row(&mut out, "saved", format!("{:.2} €", r.saving_eur()));
    row(
        &mut out,
        "…of it on the bill",
        format!("{:.2} €", r.bill_saving_eur()),
    );
    let _ = writeln!(out);
    row(
        &mut out,
        "§ 14a limit in force",
        format!("{} min", r.limited_minutes),
    );
    if r.limited_minutes > 0 {
        row(
            &mut out,
            "…against a minimum of",
            format!("{:.1} kW", r.minimum_power_kw),
        );
    }
    if r.commanded_below_minimum {
        row(
            &mut out,
            "commanded below that minimum",
            "YES — unlawful, and recorded".to_string(),
        );
    }
    if r.failsafe_below_minimum {
        row(
            &mut out,
            "own failsafe below that minimum",
            "YES — a configuration fault".to_string(),
        );
    }
    if r.lent_kwh > 0.005 {
        row(
            &mut out,
            "…covered by the store",
            format!("{:.1} kWh", r.lent_kwh),
        );
    }
    row(
        &mut out,
        "control events recorded",
        format!("{} ({} samples)", r.control_events, r.evidence_samples),
    );
    row(
        &mut out,
        "self-restraint records",
        format!("{}", r.failsafe_events),
    );
    row(
        &mut out,
        "slowest reaction",
        match r.acted_by_command {
            Some(true) => format!("{:.0} s, commanded", r.worst_latency_s),
            Some(false) => "0 s, already below".to_string(),
            None => format!("{:.0} s", r.worst_latency_s),
        },
    );
    row(
        &mut out,
        "minutes without a plan",
        format!("{}", r.minutes_without_a_plan),
    );
    // The other seam between the arbiter and the world: how often a device could
    // not hold the command it was given. A charge point is off or above 6 A with
    // nothing in between, so this is never structurally zero — and a day where it
    // is large is a day the planner was modelling a device that does not exist.
    row(
        &mut out,
        "commands the hardware clipped",
        format!("{} ticks ({:.2} kWh)", r.clipped_ticks, r.clipped_kwh),
    );
    // The § 51 EEG hours the day contained, and the carbon behind what the
    // household drew. Both are numbers the objective's own terms owe a day
    // (R20): § 51 is applied per slot inside the price stack and no day used to
    // say whether it had bound, and the carbon term could be priced with
    // nothing reporting its effect. The intensity is the *import-weighted* one
    // rather than the grid's average, because moving load from the evening ramp
    // into the middle of the day changes the first and leaves the second alone.
    row(
        &mut out,
        "quarter hours § 51 EEG zeroed",
        format!("{}", r.para51_hours),
    );
    if r.imported_co2_kg > 0.0 {
        row(
            &mut out,
            "carbon behind the imports",
            format!(
                "{:.1} kg ({:.0} g/kWh)",
                r.imported_co2_kg,
                r.imported_co2_kg / r.imported_kwh.max(1e-9) * 1000.0
            ),
        );
    }
    // What the plan that opened the day thought the day would cost, against what
    // it did. The seam between a forecast and a meter, in the currency everything
    // else in this report is in — and structurally zero for as long as the
    // planner was shown the answer, which is why it is worth printing.
    if let Some(expected) = r.opening_plan_bill_eur {
        row(
            &mut out,
            "the opening plan expected",
            format!(
                "{expected:.2} €, off by {:+.2}",
                r.cost.billed_eur() - expected
            ),
        );
    }
    if r.unmet_charge_kwh > 0.01 {
        row(
            &mut out,
            "car left short by",
            format!("{:.1} kWh", r.unmet_charge_kwh),
        );
    } else if r.planned_charge_shortfall_kwh > 0.01 {
        row(
            &mut out,
            "a plan feared falling short by",
            format!("{:.1} kWh, and did not", r.planned_charge_shortfall_kwh),
        );
    }
    row(
        &mut out,
        "without an Energy Guard",
        format!("{} min", r.failsafe_minutes),
    );
    row(
        &mut out,
        "§ 14a limit respected",
        if r.grid_event_respected {
            "yes".to_string()
        } else {
            format!("NO, by {:.0} W", r.worst_overshoot_w)
        },
    );
    // § 9 EEG is the other statutory limit on this connection point and it used
    // to have no line of its own: the peak feed-in was printed beside its
    // ceiling and left for the reader to compare, and it sat above it while the
    // § 14a line said the day had been compliant throughout. Two rules, two
    // answers.
    if r.feed_in_ceiling_kw.is_some() {
        row(
            &mut out,
            "§ 9 EEG ceiling respected",
            if r.worst_feed_in_overshoot_w <= 0.0 {
                "yes".to_string()
            } else {
                // Named for what it is: the connection point crossed the
                // ceiling between two runs of the guard, which is the control
                // period rather than a decision. A real box ticks once a
                // second; this day ticks once a minute.
                format!(
                    "{:.0} W for {} min, one control period behind a load step",
                    r.worst_feed_in_overshoot_w, r.feed_in_over_minutes
                )
            },
        );
    }
    let _ = writeln!(out);
    if r.risk_re_solves > 0 {
        row(
            &mut out,
            "re-solved against three futures",
            format!("{}×, because a service was at risk", r.risk_re_solves),
        );
    }
    row(
        &mut out,
        "described in S2",
        match r.s2_undescribed {
            0 => format!("{} resources", r.s2_resources),
            n => format!("{} resources, {n} it cannot express", r.s2_resources),
        },
    );
    if r.widest_asset_value_ratio > 1.0 {
        row(
            &mut out,
            "dearest asset vs cheapest",
            format!("{:.0}×", r.widest_asset_value_ratio),
        );
    }
    if r.relief_eur_per_kwh > 0.0 {
        row(
            &mut out,
            "relief from § 14a was worth",
            format!("{:.2} €/kWh", r.relief_eur_per_kwh),
        );
    }
    if let Some(break_even) = r.modul2_break_even_kwh_per_year {
        row(
            &mut out,
            "Modul 2 pays above",
            format!("{break_even:.0} kWh/a"),
        );
        row(
            &mut out,
            "…on this day it would have",
            format!("{:+.2} € on the energy", r.modul2_delta_today_eur),
        );
    }
    let _ = writeln!(out);
    out
}
