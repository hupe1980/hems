//! § 14a EnWG — netzorientierte Steuerung of controllable devices.
//!
//! Everything here comes from **Anlage 1 zum Beschluss BK6-22-300 vom
//! 27.11.2023** (`specs/bnetza/bk6-22-300-anlage1-20231127.pdf`), cited as
//! `[A1 <Ziffer>]`. The VDE FNN Hinweis to Tenorziffer 2f
//! (`specs/bnetza/bk6-22-300-vde-fnn-empfehlung-tenorziffer-2f.pdf`) supplies
//! the reading of the minimum-power formula.
//!
//! Three questions, three functions:
//!
//! 1. **Is this device a steuerbare Verbrauchseinrichtung?**
//!    [`classify_on`] — the Fallgruppen of `[A1 2.4.1]`, the per-Fallgruppe
//!    summation of `[A1 2.4.2]`, the 4,2 kW threshold, and — because the answer
//!    changes with the calendar — the transitional regimes of `[A1 10]`.
//! 2. **Does it have to take part, and under which regime?**
//!    [`participation`] — `[A1 3.1.b]` and the transitional rules of `[A1 10]`.
//! 3. **How far may the network operator turn it down?**
//!    [`minimum_power`] — `[A1 4.5]`, the number that decides whether a control
//!    command is lawful.

use hems_core::prelude::*;
use metering::para14a as m14a;
use time::{Date, OffsetDateTime};

pub use m14a::{Para14aConfig, Verursachungsregel};

/// The base minimum every controllable device is owed, `[A1 4.5.1 S. 1]`.
///
/// A watt view of `metering::para14a::MINDESTLEISTUNG_KW`, which is where the
/// value and its citation live.
pub const MINDESTLEISTUNG: Power = Power::new_const(4_200.0);

/// The threshold above which a heat-pump or cooling group is scaled instead of
/// being given the flat minimum, `[A1 4.5.1 S. 2]`, `[A1 4.5.2 S. 3]`.
pub const SCALING_THRESHOLD: Power = Power::new_const(11_000.0);

/// The scaling factor presumed appropriate until a different recommendation
/// takes effect, `[A1 4.5.1 S. 3]`.
pub const SKALIERUNGSFAKTOR: f64 = 0.4;

/// The power above which a device is controllable at all, `[A1 2.4.1]`.
///
/// Strictly: `[A1 2.4.1]` admits a device *"mit einer Netzanschlussleistung von
/// **mehr als** 4,2 kW"*, so exactly 4,2 kW is not one.
pub const FALLGRUPPE_THRESHOLD: Power = Power::new_const(4_200.0);

/// Kilowatts as an exact decimal, for the `metering` calls below.
///
/// Six places is a milliwatt — four orders of magnitude finer than anything a
/// household device resolves, and the same tolerance the compliance record
/// works to. The § 14a arithmetic is a table lookup and three multiplications
/// evaluated once per verdict, so routing it through exact decimals costs
/// nothing measurable and buys one implementation of a regulation instead of
/// two that can disagree.
fn to_kw(power: Power) -> rust_decimal::Decimal {
    rust_decimal::Decimal::from_f64_retain(power.max(Power::ZERO).kw())
        .unwrap_or_default()
        .round_dp(6)
}

/// The inverse of [`to_kw`].
fn from_kw(kw: rust_decimal::Decimal) -> Power {
    use rust_decimal::prelude::ToPrimitive;
    Power::from_kw(kw.to_f64().unwrap_or(0.0))
}

/// The last day of the old world: a device commissioned after this date must
/// take part, `[A1 3.1.b]`.
pub const LEGACY_CUTOFF: Date = time::macros::date!(2023 - 12 - 31);

/// Legacy network-fee reductions run out at the end of 2028, `[A1 10.1]`.
pub const LEGACY_REGIME_END: Date = time::macros::date!(2028 - 12 - 31);

/// How the network operator addresses the devices behind one connection,
/// `[A1 4.4]`. The customer chooses, per device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ControlMode {
    /// `[A1 4.4.a]` — every device gets its own setpoint and its own minimum.
    Direct,
    /// `[A1 4.4.b]` — one setpoint for the sum of everything behind the energy
    /// management system, which the operator may then distribute as it sees fit
    /// (`[A1 4.5.2 S. 6]`). This is the mode a HEMS exists for.
    #[default]
    Ems,
}

/// One controllable device, or one summed Fallgruppe treated as a device.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SteuVe {
    /// The assets it is made of. More than one only for a summed Fallgruppe
    /// under `[A1 2.4.2]`.
    pub assets: Vec<AssetId>,
    /// Which Fallgruppe of `[A1 2.4.1]`.
    pub fallgruppe: Fallgruppe,
    /// The Netzanschlussleistung the thresholds and the scaling apply to.
    pub power: Power,
}

impl SteuVe {
    /// The minimum this device is owed under direct control, `[A1 4.5.1]`.
    ///
    /// Flat 4,2 kW, except for a heat-pump or cooling group whose connection
    /// power exceeds 11 kW: there the minimum scales with the installation, so a
    /// large heat pump keeps a usable fraction of its power rather than the same
    /// 4,2 kW as a small one.
    #[must_use]
    pub fn minimum_power_direct(&self) -> Power {
        m14a::mindestleistung_direktansteuerung(&self.as_metering(), &Para14aConfig::default())
            .map_or(Power::ZERO, from_kw)
    }

    /// This device as `metering` counts it.
    fn as_metering(&self) -> m14a::SteuVe {
        m14a::SteuVe::new(self.fallgruppe, to_kw(self.power))
    }
}

/// The Gleichzeitigkeitsfaktor for `n` devices under EMS control, `[A1 4.5.2]`.
///
/// The table stops at nine because the factor does: from the ninth device on it
/// stays at 0,45. For fewer than two devices the factor never appears in the
/// formula — the `(n − 1)` term is zero — so the value returned there is only
/// there to keep the function total.
#[must_use]
pub fn gleichzeitigkeitsfaktor(n: usize) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    u32::try_from(n)
        .ok()
        .and_then(m14a::gleichzeitigkeitsfaktor)
        .and_then(|d| d.to_f64())
        .unwrap_or(1.0)
}

/// The minimum power the operator may reduce a connection to, `[A1 4.5]`.
///
/// Under [`ControlMode::Ems`] this is one number for **all** controllable
/// devices behind the energy management system, and `[A1 4.5.2 S. 6]` lets the
/// customer spend it as they like — charge the car and let the heat pump wait,
/// or the other way round. Under [`ControlMode::Direct`] every device carries
/// its own minimum and the sum is only informational.
///
/// The formula, for `n` devices:
///
/// ```text
/// P_min = base + (n − 1) × GZF(n) × 4,2 kW
///
/// base = max(0,4 × ΣP_Wärmepumpe ; 0,4 × ΣP_Raumkühlung)   if any heat-pump or
///                                                            cooling group above
///                                                            11 kW is included
///      = 4,2 kW                                              otherwise
/// ```
///
/// The arithmetic itself is `metering::para14a`'s, not a second copy of it.
/// This function's job is to take the *site's* view — assets grouped into
/// controllable devices by [`classify_on`], powers in watts — and ask the
/// question in the Festlegung's own terms.
#[must_use]
pub fn minimum_power(devices: &[SteuVe], mode: ControlMode) -> Power {
    if devices.is_empty() {
        return Power::ZERO;
    }
    match mode {
        ControlMode::Direct => devices.iter().map(SteuVe::minimum_power_direct).sum(),
        ControlMode::Ems => {
            let grouped: Vec<m14a::SteuVe> = devices.iter().map(SteuVe::as_metering).collect();
            m14a::mindestleistung_ems(&grouped, &Para14aConfig::default())
                .map_or(Power::ZERO, from_kw)
        }
    }
}

/// Whether one asset is under the netzorientierte Steuerung on `date`.
///
/// The three questions of `[A1 3.1.b]` and `[A1 10]` asked of a single asset:
/// is it exempt, was it commissioned after 31.12.2023, and if not, which
/// transitional regime is it in and has that regime run out yet.
#[must_use]
pub fn is_controlled_on(asset: &Asset, date: Date) -> bool {
    let meta = asset.meta();
    participation(
        meta.commissioned_at,
        meta.steuve_exemption,
        meta.legacy_status,
        meta.switched_voluntarily,
    )
    .is_controlled_on(date)
}

/// Group the assets of a site into controllable devices, `[A1 2.4]`.
///
/// Charge points and storage are counted one by one; heat pumps and cooling are
/// **summed per Fallgruppe** behind the connection first, and if the sum passes
/// 4,2 kW the whole group counts as a single controllable device
/// (`[A1 2.4.2]`). Two 3 kW heat pumps are therefore one 6 kW device, not two
/// devices below the threshold — a distinction worth 4,2 kW of minimum power.
///
/// # Only devices that are actually under control
///
/// `date` is the day the question is being asked on, and it is not decoration.
/// `[A1 3.1.b]` binds devices commissioned after 31.12.2023; `[A1 10]` leaves an
/// older one either out of scope entirely (`[A1 10.3]`), on the old reduction
/// until 31.12.2028 (`[A1 10.1]`), or — for a night-storage heater — on it
/// indefinitely (`[A1 10.2.b]`). Treating such a device as a controllable one
/// does two wrong things at once: the network operator gets a share of its power
/// it has no right to reduce, and the device's consumption is counted as
/// netzwirksamer Leistungsbezug when it is ordinary load. Both make the house
/// *more* curtailed than the Festlegung allows.
///
/// [`participation`] answers the question per asset and is the only place the
/// dates live.
#[must_use]
pub fn classify_on(assets: &[Asset], date: Date) -> Vec<SteuVe> {
    let mut out = Vec::new();
    let controlled = |a: &&Asset| is_controlled_on(a, date);
    let group = |fallgruppe: Fallgruppe| -> Option<SteuVe> {
        let members: Vec<&Asset> = assets
            .iter()
            .filter(|a| a.fallgruppe() == Some(fallgruppe))
            .filter(controlled)
            .collect();
        let power: Power = members.iter().map(|a| a.steuve_power()).sum();
        (power > FALLGRUPPE_THRESHOLD && !members.is_empty()).then(|| SteuVe {
            assets: members.iter().map(|a| a.id().clone()).collect(),
            fallgruppe,
            power,
        })
    };

    // b. and c. are summed per Fallgruppe.
    out.extend(group(Fallgruppe::Waermepumpe));
    out.extend(group(Fallgruppe::Raumkuehlung));

    // a. and d. are counted individually.
    for asset in assets {
        let Some(fallgruppe) = asset.fallgruppe() else {
            continue;
        };
        if !matches!(
            fallgruppe,
            Fallgruppe::Ladepunkt | Fallgruppe::Stromspeicher
        ) {
            continue;
        }
        if !is_controlled_on(asset, date) {
            continue;
        }
        if asset.steuve_power() > FALLGRUPPE_THRESHOLD {
            out.push(SteuVe {
                assets: vec![asset.id().clone()],
                fallgruppe,
                power: asset.steuve_power(),
            });
        }
    }
    out
}

/// [`classify_on`] at an instant, in the Europe/Berlin calendar the Festlegung
/// is written in.
///
/// A regime boundary is a calendar date (`[A1 10.1]`: the old rules run to
/// 31.12.2028), so the hour between Berlin midnight and UTC midnight belongs to
/// the day the household is living in, not the one the clock is set to.
#[must_use]
pub fn classify_at(assets: &[Asset], now: OffsetDateTime) -> Vec<SteuVe> {
    classify_on(assets, metering::calendar::local_day(now))
}

/// The **netzwirksamer Leistungsbezug**, `[A1 2.3]`.
///
/// Not "what the controllable devices are drawing" but "the part of what the
/// connection is drawing *from the grid* that they cause". The distinction is
/// the whole economic case for an energy management system under § 14a: while
/// the photovoltaic system produces more than the rest of the house needs, the
/// surplus covers the wallbox, and the wallbox can keep charging above the
/// network operator's limit without a single watt of it being netzwirksam.
///
/// # The Festlegung does not say how to split it
///
/// `[A1 2.3]` says *"derjenige Anteil … der zeitgleich durch eine oder mehrere
/// steuerbare Verbrauchseinrichtungen verursacht wird"* and stops. When local
/// generation covers part of the load, **which** part of the remaining grid draw
/// the controllable devices caused is an apportionment the text does not
/// perform — so this does not silently perform one either. It takes a
/// [`Verursachungsregel`]:
///
/// * [`SteuVeZuletzt`](Verursachungsregel::SteuVeZuletzt) gives the surplus to
///   the **other** load first, so whatever grid draw is left is the controllable
///   devices'. It can never understate their share, so a guard built on it errs
///   early rather than late, and it is the default.
/// * [`Anteilig`](Verursachungsregel::Anteilig) shares the generation pro rata.
///   It is lower whenever a roof is producing, so a household may only run it
///   where its network operator's Technische Mindestanforderungen say so.
///
/// All three power arguments are non-negative magnitudes.
#[must_use]
pub fn netzwirksamer_leistungsbezug_by(
    steuve_consumption: Power,
    other_consumption: Power,
    local_generation: Power,
    regel: Verursachungsregel,
) -> Power {
    // The grid draw the connection point would see, which is what `metering`
    // asks for: everything the house is taking, less what it is making.
    let netzbezug = steuve_consumption + other_consumption - local_generation;
    m14a::netzwirksamer_leistungsbezug(
        to_kw(netzbezug),
        to_kw(steuve_consumption),
        Some(to_kw(other_consumption)),
        regel,
    )
    .map_or(Power::ZERO, from_kw)
}

/// [`netzwirksamer_leistungsbezug_by`] under the conservative convention.
#[must_use]
pub fn netzwirksamer_leistungsbezug(
    steuve_consumption: Power,
    other_consumption: Power,
    local_generation: Power,
) -> Power {
    netzwirksamer_leistungsbezug_by(
        steuve_consumption,
        other_consumption,
        local_generation,
        Verursachungsregel::SteuVeZuletzt,
    )
}

/// How much the controllable devices may consume without exceeding `ceiling`.
///
/// The inverse of [`netzwirksamer_leistungsbezug`] under
/// [`Verursachungsregel::SteuVeZuletzt`]: the limit plus whatever local surplus
/// is available after the rest of the house has been served.
///
/// There is no inverse for [`Verursachungsregel::Anteilig`] and there cannot be
/// a useful one: under that convention the share depends on the controllable
/// draw itself, so "how much may they take" is a fixed point rather than a sum.
/// A household on the pro-rata convention gets a budget computed the
/// conservative way and a *measurement* computed its own way, which errs
/// towards leaving the operator's limit alone.
#[must_use]
pub fn steuve_budget(ceiling: Power, other_consumption: Power, local_generation: Power) -> Power {
    let surplus = (local_generation - other_consumption).max(Power::ZERO);
    (ceiling + surplus).max(Power::ZERO)
}

/// What a device's history says about its obligations, `[A1 3.1.b]` and `[A1 10]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind"))]
pub enum Participation {
    /// Commissioned after 31.12.2023: takes part, `[A1 3.1.b]`.
    Mandatory,
    /// An old device on the old reduced network fee. The old rules run until the
    /// given date, then the new ones apply, `[A1 10.1]`, `[A1 10.2.a]`.
    Legacy {
        /// The last day the old regime covers.
        until: Date,
    },
    /// A night-storage heater: the old rule continues until the contract ends or
    /// the device is decommissioned — no end date, `[A1 10.2.b]`.
    Nachtspeicher,
    /// Commissioned before 2024 and never on the old reduced fee: this
    /// Festlegung does not apply at all, `[A1 10.3]`.
    OutOfScope,
    /// Nobody has said when it was commissioned, so hems assumes it takes part.
    ///
    /// The asymmetry is deliberate and it runs the *opposite* way to the
    /// contractual reading. Leaving an unknown device out of the group is how a
    /// site exceeds a network operator's limit — the guard would hand its share
    /// of the budget to the others while it kept drawing — and it also *lowers*
    /// `P_min`, because the minimum of `[A1 4.5.2]` grows with the number of
    /// devices. Guessing "in" is therefore better for compliance **and** better
    /// for the customer, and it is one edit away from being corrected.
    UnknownAssumedControlled,
    /// The customer switched voluntarily into the netzorientierte Steuerung.
    /// The operator cannot refuse, and the move cannot be undone, `[A1 10.4]`.
    SwitchedVoluntarily,
    /// Excluded by `[A1 3.1.b aa/bb]`.
    Exempt {
        /// Which exclusion.
        reason: SteuVeExemption,
    },
}

impl Participation {
    /// Whether the netzorientierte Steuerung of `[A1 4]` applies on `date`.
    #[must_use]
    pub fn is_controlled_on(&self, date: Date) -> bool {
        match self {
            Participation::Mandatory
            | Participation::SwitchedVoluntarily
            | Participation::UnknownAssumedControlled => true,
            Participation::Legacy { until } => date > *until,
            Participation::Nachtspeicher
            | Participation::OutOfScope
            | Participation::Exempt { .. } => false,
        }
    }
}

/// Decide a device's regime.
///
/// `switched_voluntarily` records the irreversible move of `[A1 10.4]`: an
/// operator of a legacy or out-of-scope device may enter the netzorientierte
/// Steuerung at any time, the network operator may not refuse, and there is no
/// way back. hems stores the flag rather than recomputing it, because the
/// decision is a contractual fact, not a derivation.
///
/// An **unknown** commissioning date yields
/// [`Participation::UnknownAssumedControlled`]: the device takes part until
/// somebody says otherwise. Reading silence as "old" —/// did — is the answer that quietly drops a device out of the § 14a group, and
/// dropping a device out of the group is how a site exceeds a limit. It is also
/// worse for the customer, because `P_min` grows with the number of devices. A
/// device only leaves the group on a positive statement: an exemption, or a
/// commissioning date before 2024 with a known legacy status.
#[must_use]
pub fn participation(
    commissioned_at: Option<Date>,
    exemption: Option<SteuVeExemption>,
    legacy: LegacyStatus,
    switched_voluntarily: bool,
) -> Participation {
    if let Some(reason) = exemption {
        return Participation::Exempt { reason };
    }
    let Some(commissioned_at) = commissioned_at else {
        return match legacy {
            LegacyStatus::Nachtspeicher => Participation::Nachtspeicher,
            _ => Participation::UnknownAssumedControlled,
        };
    };
    if commissioned_at > LEGACY_CUTOFF {
        return Participation::Mandatory;
    }
    if switched_voluntarily {
        return Participation::SwitchedVoluntarily;
    }
    match legacy {
        LegacyStatus::Nachtspeicher => Participation::Nachtspeicher,
        LegacyStatus::ReducedNetworkFee => Participation::Legacy {
            until: LEGACY_REGIME_END,
        },
        LegacyStatus::None => Participation::OutOfScope,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::asset::{AssetMeta, Evse, HeatPump, HeatPumpControl};

    fn meta(id: &str, kw: f64) -> AssetMeta {
        AssetMeta::new(
            AssetId::new(id).unwrap(),
            CircuitId::new("main").unwrap(),
            PhaseConnection::Three,
            Power::from_kw(kw),
        )
    }

    fn heat_pump(id: &str, kw: f64) -> Asset {
        Asset::HeatPump(HeatPump {
            meta: meta(id, kw),
            electrical_nominal: Power::from_kw(kw),
            heating_rod: None,
            control: HeatPumpControl::PowerCeiling,
            modulating: true,
        })
    }

    fn wallbox(id: &str, kw: f64) -> Asset {
        Asset::Evse(Evse {
            meta: meta(id, kw),
            min_current: Current::new(6.0),
            max_current: Current::new(16.0),
            bidirectional: false,
            public: false,
        })
    }

    fn steuve(fallgruppe: Fallgruppe, kw: f64) -> SteuVe {
        SteuVe {
            assets: vec![AssetId::new("x").unwrap()],
            fallgruppe,
            power: Power::from_kw(kw),
        }
    }

    // ── [A1 4.5.2] the worked cases ────────────────────────────────────────

    #[test]
    fn the_two_causation_conventions_differ_only_while_a_roof_is_producing() {
        // `[A1 2.3]` says which quantity is limited and not how to split it
        // when local generation covers part of the load. Both readings are
        // defensible, hems defaults to the conservative one, and the household
        // has to be told which it is running.
        let steuve = Power::from_kw(6.0);
        let other = Power::from_kw(8.0);

        // Nothing on the roof: the two agree, because there is nothing to split.
        for regel in Verursachungsregel::ALL {
            assert_eq!(
                netzwirksamer_leistungsbezug_by(steuve, other, Power::ZERO, regel),
                steuve,
                "{regel:?} with no generation"
            );
        }

        // 4 kW of production against 14 kW of load: 10 kW still comes from the
        // grid. Conservatively all 6 kW of it is the wallbox's; pro rata it
        // carries 6/14 of the 10 kW.
        let pv = Power::from_kw(4.0);
        let conservative =
            netzwirksamer_leistungsbezug_by(steuve, other, pv, Verursachungsregel::SteuVeZuletzt);
        let pro_rata =
            netzwirksamer_leistungsbezug_by(steuve, other, pv, Verursachungsregel::Anteilig);
        assert!((conservative.kw() - 6.0).abs() < 1e-6, "got {conservative}");
        assert!((pro_rata.kw() - 4.285_714).abs() < 1e-3, "got {pro_rata}");
        assert!(
            pro_rata < conservative,
            "the default is never the lower one"
        );
    }

    #[test]
    fn a_surplus_larger_than_the_controllable_draw_leaves_nothing_netzwirksam() {
        let n = netzwirksamer_leistungsbezug(
            Power::from_kw(6.0),
            Power::from_kw(1.0),
            Power::from_kw(9.0),
        );
        assert_eq!(n, Power::ZERO);
    }

    #[test]
    fn one_device_under_ems_gets_the_flat_minimum() {
        let p = minimum_power(&[steuve(Fallgruppe::Ladepunkt, 11.0)], ControlMode::Ems);
        assert_eq!(p, MINDESTLEISTUNG);
    }

    #[test]
    fn the_gzf_table_matches_the_festlegung() {
        for (n, expected) in [
            (2, 0.80),
            (3, 0.75),
            (4, 0.70),
            (5, 0.65),
            (6, 0.60),
            (7, 0.55),
            (8, 0.50),
        ] {
            assert!(
                (gleichzeitigkeitsfaktor(n) - expected).abs() < 1e-12,
                "n = {n}"
            );
        }
        for n in 9..20 {
            assert!((gleichzeitigkeitsfaktor(n) - 0.45).abs() < 1e-12, "n = {n}");
        }
    }

    #[test]
    fn two_ordinary_devices_under_ems() {
        // 4,2 + (2 − 1) × 0,8 × 4,2 = 7,56 kW
        let devices = [
            steuve(Fallgruppe::Ladepunkt, 11.0),
            steuve(Fallgruppe::Stromspeicher, 5.0),
        ];
        let p = minimum_power(&devices, ControlMode::Ems);
        assert!((p.kw() - 7.56).abs() < 1e-9, "got {p}");
    }

    #[test]
    fn a_large_heat_pump_replaces_the_base_term() {
        // 20 kW heat pump + wallbox: base = 0,4 × 20 = 8 kW, then + 1 × 0,8 × 4,2.
        let devices = [
            steuve(Fallgruppe::Waermepumpe, 20.0),
            steuve(Fallgruppe::Ladepunkt, 11.0),
        ];
        let p = minimum_power(&devices, ControlMode::Ems);
        assert!((p.kw() - (8.0 + 3.36)).abs() < 1e-9, "got {p}");
    }

    #[test]
    fn the_base_takes_the_larger_of_heat_and_cooling_not_their_sum() {
        // [A1 4.5.2 S. 3] says max(0,4 × ΣWP ; 0,4 × ΣKlima) — heating and
        // cooling do not run at full power at the same time, so adding them
        // would hand out a minimum the network never has to grant.
        let devices = [
            steuve(Fallgruppe::Waermepumpe, 20.0),
            steuve(Fallgruppe::Raumkuehlung, 15.0),
        ];
        let p = minimum_power(&devices, ControlMode::Ems);
        assert!((p.kw() - (8.0 + 0.8 * 4.2)).abs() < 1e-9, "got {p}");
    }

    #[test]
    fn a_thermal_group_at_exactly_eleven_kw_is_not_scaled() {
        // "über 11 kW" is strict: at 11,0 kW the flat minimum still applies.
        let devices = [steuve(Fallgruppe::Waermepumpe, 11.0)];
        assert_eq!(minimum_power(&devices, ControlMode::Ems), MINDESTLEISTUNG);
        assert_eq!(devices[0].minimum_power_direct(), MINDESTLEISTUNG);
    }

    #[test]
    fn direct_control_gives_each_device_its_own_minimum() {
        let devices = [
            steuve(Fallgruppe::Ladepunkt, 11.0),
            steuve(Fallgruppe::Waermepumpe, 20.0),
        ];
        // 4,2 for the charge point, 0,4 × 20 = 8 for the heat pump.
        assert!((minimum_power(&devices, ControlMode::Direct).kw() - 12.2).abs() < 1e-9);
    }

    #[test]
    fn ems_control_is_never_worse_for_the_customer_than_one_device_alone() {
        // The whole point of the EMS mode: more devices can only raise the floor.
        let mut previous = Power::ZERO;
        let mut devices = Vec::new();
        for i in 0..12 {
            devices.push(steuve(Fallgruppe::Ladepunkt, 11.0));
            let p = minimum_power(&devices, ControlMode::Ems);
            assert!(
                p >= previous,
                "adding device {i} lowered the minimum: {previous} → {p}"
            );
            previous = p;
        }
    }

    // ── [A1 2.4] classification ────────────────────────────────────────────

    /// A day well inside the new regime, used by every classification test.
    const TODAY: Date = time::macros::date!(2026 - 08 - 30);

    #[test]
    fn two_small_heat_pumps_are_summed_into_one_controllable_device() {
        let devices = classify_on(&[heat_pump("wp1", 3.0), heat_pump("wp2", 3.0)], TODAY);
        assert_eq!(devices.len(), 1, "[A1 2.4.2] sums the Fallgruppe");
        assert_eq!(devices[0].fallgruppe, Fallgruppe::Waermepumpe);
        assert_eq!(devices[0].power, Power::from_kw(6.0));
        assert_eq!(devices[0].assets.len(), 2);
    }

    #[test]
    fn two_small_charge_points_are_not_summed() {
        // [A1 2.4.2] names only Fallgruppen b. and c.
        assert!(classify_on(&[wallbox("wb1", 3.7), wallbox("wb2", 3.7)], TODAY).is_empty());
    }

    #[test]
    fn a_device_at_the_threshold_is_not_controllable() {
        // "mehr als 4,2 Kilowatt" — 4,2 exactly is not more than 4,2.
        assert!(classify_on(&[wallbox("wb", 4.2)], TODAY).is_empty());
        assert_eq!(classify_on(&[wallbox("wb", 4.3)], TODAY).len(), 1);
    }

    #[test]
    fn an_exempt_device_drops_out_of_its_group() {
        let mut public = wallbox("wb", 11.0);
        if let Asset::Evse(e) = &mut public {
            e.public = true;
        }
        assert!(classify_on(&[public], TODAY).is_empty());
    }

    #[test]
    fn a_heating_rod_counts_towards_its_heat_pumps_group() {
        let hp = Asset::HeatPump(HeatPump {
            meta: meta("wp", 3.0),
            electrical_nominal: Power::from_kw(3.0),
            heating_rod: Some(Power::from_kw(9.0)),
            control: HeatPumpControl::SgReady,
            modulating: false,
        });
        let devices = classify_on(&[hp], TODAY);
        assert_eq!(devices[0].power, Power::from_kw(12.0));
        // …and 12 kW is above 11, so the minimum scales: 0,4 × 12 = 4,8 kW.
        assert!((devices[0].minimum_power_direct().kw() - 4.8).abs() < 1e-9);
    }

    #[test]
    fn a_device_from_before_2024_with_no_old_reduction_is_not_classified_at_all() {
        // [A1 10.3]. The bug this pins: `participation` existed, was tested, and
        // nothing called it — so an out-of-scope wallbox was handed to the guard
        // as a controllable device, its consumption counted as netzwirksamer
        // Leistungsbezug, and the house curtailed for a rule that does not apply
        // to it.
        let mut old = wallbox("wb", 11.0);
        if let Asset::Evse(e) = &mut old {
            e.meta.commissioned_at = Some(time::macros::date!(2019 - 03 - 01));
        }
        assert!(classify_on(std::slice::from_ref(&old), TODAY).is_empty());

        // …until the operator opts in, which is irreversible, `[A1 10.4]`.
        if let Asset::Evse(e) = &mut old {
            e.meta.switched_voluntarily = true;
        }
        assert_eq!(classify_on(&[old], TODAY).len(), 1);
    }

    #[test]
    fn a_legacy_device_joins_the_group_only_in_2029() {
        // [A1 10.1]: the old reduced network fee runs to 31.12.2028.
        let mut legacy = heat_pump("wp", 9.0);
        if let Asset::HeatPump(hp) = &mut legacy {
            hp.meta.commissioned_at = Some(time::macros::date!(2021 - 05 - 01));
            hp.meta.legacy_status = LegacyStatus::ReducedNetworkFee;
        }
        let new = wallbox("wb", 11.0);
        let assets = [legacy, new];

        // Today only the wallbox is controllable, so the customer is owed the
        // flat minimum for one device…
        let today = classify_on(&assets, TODAY);
        assert_eq!(today.len(), 1);
        assert_eq!(minimum_power(&today, ControlMode::Ems), MINDESTLEISTUNG);

        // …and on new year's day 2029 the heat pump joins, which raises it.
        let later = classify_on(&assets, time::macros::date!(2029 - 01 - 01));
        assert_eq!(later.len(), 2);
        assert!(minimum_power(&later, ControlMode::Ems) > MINDESTLEISTUNG);
    }

    #[test]
    fn a_night_storage_heater_never_joins_by_itself() {
        let mut nsh = heat_pump("nsh", 9.0);
        if let Asset::HeatPump(hp) = &mut nsh {
            hp.meta.commissioned_at = Some(time::macros::date!(1998 - 01 - 01));
            hp.meta.legacy_status = LegacyStatus::Nachtspeicher;
        }
        assert!(classify_on(std::slice::from_ref(&nsh), TODAY).is_empty());
        assert!(classify_on(&[nsh], time::macros::date!(2035 - 01 - 01)).is_empty());
    }

    #[test]
    fn an_untouched_group_member_does_not_drag_an_exempt_one_in() {
        // Two heat pumps of 3 kW each are one 6 kW device `[A1 2.4.2]` — but only
        // if both are actually under control. Take one out and the group falls
        // back under the threshold.
        let mut old = heat_pump("wp1", 3.0);
        if let Asset::HeatPump(hp) = &mut old {
            hp.meta.commissioned_at = Some(time::macros::date!(2015 - 01 - 01));
        }
        let mut new = heat_pump("wp2", 3.0);
        if let Asset::HeatPump(hp) = &mut new {
            hp.meta.commissioned_at = Some(time::macros::date!(2025 - 01 - 01));
        }
        assert!(classify_on(&[old, new], TODAY).is_empty());
    }

    #[test]
    fn the_berlin_calendar_decides_which_day_a_regime_ends_on() {
        // 2029-01-01 00:30 Berlin is 2028-12-31 23:30 UTC. The Festlegung is
        // written in calendar dates, and the household lives in Berlin.
        let mut legacy = wallbox("wb", 11.0);
        if let Asset::Evse(e) = &mut legacy {
            e.meta.commissioned_at = Some(time::macros::date!(2020 - 01 - 01));
            e.meta.legacy_status = LegacyStatus::ReducedNetworkFee;
        }
        let assets = [legacy];
        assert!(classify_at(&assets, time::macros::datetime!(2028-12-31 22:30:00 UTC)).is_empty());
        assert_eq!(
            classify_at(&assets, time::macros::datetime!(2028-12-31 23:30:00 UTC)).len(),
            1
        );
    }

    // ── [A1 2.3] netzwirksamer Leistungsbezug ─────────────────────────────

    #[test]
    fn without_generation_everything_the_devices_draw_is_netzwirksam() {
        let n =
            netzwirksamer_leistungsbezug(Power::from_kw(11.0), Power::from_kw(0.5), Power::ZERO);
        assert_eq!(n, Power::from_kw(11.0));
    }

    #[test]
    fn surplus_generation_covers_the_controllable_devices_last() {
        // 10 kW of PV, 2 kW household, 11 kW wallbox: 8 kW of surplus is left
        // after the household, so only 3 kW of the wallbox is netzwirksam.
        let n = netzwirksamer_leistungsbezug(
            Power::from_kw(11.0),
            Power::from_kw(2.0),
            Power::from_kw(10.0),
        );
        assert_eq!(n, Power::from_kw(3.0));
    }

    #[test]
    fn generation_beyond_the_whole_load_leaves_nothing_netzwirksam() {
        let n = netzwirksamer_leistungsbezug(
            Power::from_kw(4.0),
            Power::from_kw(1.0),
            Power::from_kw(20.0),
        );
        assert_eq!(n, Power::ZERO);
    }

    #[test]
    fn the_budget_is_the_limit_plus_the_surplus() {
        // This is why a household with photovoltaics can keep charging through a
        // § 14a reduction: a 4,2 kW limit plus 6 kW of surplus is 10,2 kW of
        // lawful charging.
        let budget = steuve_budget(
            Power::from_kw(4.2),
            Power::from_kw(1.0),
            Power::from_kw(7.0),
        );
        assert_eq!(budget, Power::from_kw(10.2));
        // …and the reverse holds.
        assert_eq!(
            netzwirksamer_leistungsbezug(budget, Power::from_kw(1.0), Power::from_kw(7.0)),
            Power::from_kw(4.2)
        );
    }

    #[test]
    fn a_zero_limit_still_permits_using_the_surplus() {
        assert_eq!(
            steuve_budget(Power::ZERO, Power::ZERO, Power::from_kw(5.0)),
            Power::from_kw(5.0)
        );
    }

    // ── [A1 3, 10] regimes ────────────────────────────────────────────────

    #[test]
    fn a_device_commissioned_in_2024_must_take_part() {
        let p = participation(
            Some(time::macros::date!(2024 - 01 - 01)),
            None,
            LegacyStatus::None,
            false,
        );
        assert_eq!(p, Participation::Mandatory);
        assert!(p.is_controlled_on(time::macros::date!(2026 - 08 - 30)));
    }

    #[test]
    fn a_legacy_device_changes_regime_on_new_years_day_2029() {
        let p = participation(
            Some(time::macros::date!(2020 - 06 - 01)),
            None,
            LegacyStatus::ReducedNetworkFee,
            false,
        );
        assert_eq!(
            p,
            Participation::Legacy {
                until: LEGACY_REGIME_END
            }
        );
        assert!(!p.is_controlled_on(time::macros::date!(2028 - 12 - 31)));
        assert!(p.is_controlled_on(time::macros::date!(2029 - 01 - 01)));
    }

    #[test]
    fn a_night_storage_heater_never_enters_the_new_regime_by_itself() {
        let p = participation(None, None, LegacyStatus::Nachtspeicher, false);
        assert_eq!(p, Participation::Nachtspeicher);
        assert!(!p.is_controlled_on(time::macros::date!(2030 - 01 - 01)));
    }

    #[test]
    fn an_old_device_without_a_reduction_is_simply_out_of_scope() {
        let p = participation(
            Some(time::macros::date!(2019 - 03 - 01)),
            None,
            LegacyStatus::None,
            false,
        );
        assert_eq!(p, Participation::OutOfScope);
    }

    #[test]
    fn switching_voluntarily_puts_an_old_device_under_control() {
        let p = participation(
            Some(time::macros::date!(2019 - 03 - 01)),
            None,
            LegacyStatus::None,
            true,
        );
        assert_eq!(p, Participation::SwitchedVoluntarily);
        assert!(p.is_controlled_on(time::macros::date!(2026 - 01 - 01)));
    }

    #[test]
    fn an_unknown_commissioning_date_puts_the_device_in_the_group() {
        // Both directions of the asymmetry in one test. Silence means "in":
        assert_eq!(
            participation(None, None, LegacyStatus::None, false),
            Participation::UnknownAssumedControlled
        );
        assert!(
            participation(None, None, LegacyStatus::None, false)
                .is_controlled_on(time::macros::date!(2026 - 08 - 30))
        );
        // …and a positive statement is what takes it back out.
        assert_eq!(
            participation(
                Some(time::macros::date!(2019 - 03 - 01)),
                None,
                LegacyStatus::None,
                false
            ),
            Participation::OutOfScope
        );
    }

    #[test]
    fn an_unknown_date_never_lowers_the_minimum_the_customer_is_owed() {
        // The formula of [A1 4.5.2] grows with the number of devices, so
        // guessing a device *out* of the group would reduce the floor the
        // network operator may not go below. Guessing it in cannot.
        let known = [
            steuve(Fallgruppe::Ladepunkt, 11.0),
            steuve(Fallgruppe::Waermepumpe, 8.0),
        ];
        let one = [steuve(Fallgruppe::Ladepunkt, 11.0)];
        assert!(minimum_power(&known, ControlMode::Ems) > minimum_power(&one, ControlMode::Ems));
    }

    #[test]
    fn an_exemption_beats_everything_including_a_new_commissioning_date() {
        let p = participation(
            Some(time::macros::date!(2025 - 05 - 01)),
            Some(SteuVeExemption::EmergencyServices),
            LegacyStatus::None,
            true,
        );
        assert_eq!(
            p,
            Participation::Exempt {
                reason: SteuVeExemption::EmergencyServices
            }
        );
        assert!(!p.is_controlled_on(time::macros::date!(2026 - 01 - 01)));
    }
}
