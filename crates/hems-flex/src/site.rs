//! Every S2 description one household would send, in one call.
//!
//! The individual `describe_*` functions in [`crate::describe`] each answer for
//! one device. This answers for a **site**, and the difference is not
//! convenience: it is the only way the flexibility model can be a dependency
//! rather than a document.
//!
//! A description function with no caller is the failure mode this workspace is
//! most prone to — implemented, cited, tested, and reached by nothing, which no
//! property test catches because a property is a statement about code that runs.
//! So the reference day describes its whole household through here every run and
//! reports the count, and [`SiteDescription::undescribed`] names anything the
//! mapping claimed a control type for and the crate could not actually express.
//! A number that goes up is a device that arrived; a number in `undescribed` is
//! a `describe_*` that has not been written yet, and it says so out loud instead
//! of being invisible.
//!
//! # It builds messages, it does not send them
//!
//! Everything here is a pure function of the site and an instant. Driving an
//! actual session — the handshake, `SelectControlType`, the status reports, the
//! instruction acknowledgements — needs a socket, and belongs with the daemon
//! that owns one.

use std::collections::BTreeMap;

use hems_core::prelude::*;
use s2energy::common::{Message, ResourceManagerDetails};
use s2energy::pebc;
use time::OffsetDateTime;

use crate::describe::{
    BatteryDescription, DhwDescription, EvDescription, EvStorage, HeatPumpDescription,
    ProgrammeDescription, describe_battery, describe_dhw, describe_ev, describe_evse,
    describe_heat_pump, describe_programme, describe_pv, resource_manager_details,
};
use crate::map::{ControlType, control_type_for};

/// What the descriptions need to know that the site itself does not say.
#[derive(Debug, Clone)]
pub struct DescribeContext<'a> {
    /// The instant the descriptions become valid.
    pub valid_from: OffsetDateTime,
    /// The end of the window a [`LoadKind::Shiftable`] programme has to run in.
    ///
    /// PPBC's profile carries a start and an end, and a household that says
    /// nothing means "by the end of the horizon" — the same reading
    /// [`hems_optimizer::ShiftableRun::deadline`] takes.
    ///
    /// [`hems_optimizer::ShiftableRun::deadline`]: https://docs.rs/hems-optimizer
    pub until: OffsetDateTime,
    /// The car on the charge point, when there is one with a departure time.
    ///
    /// Its presence is what turns the charge point from an envelope into a
    /// store, so it is the session rather than a flag: a description that says
    /// "this is a store" and cannot say how full it is has told a Customer
    /// Energy Manager nothing it can plan with.
    pub ev_session: Option<EvStorage>,
    /// Which conductors each switchable asset is using right now. An absent
    /// entry means the mode its wiring implies.
    pub modes: &'a BTreeMap<AssetId, PhaseMode>,
}

impl<'a> DescribeContext<'a> {
    /// A context with no switchable device in a non-default mode.
    #[must_use]
    pub fn new(
        valid_from: OffsetDateTime,
        until: OffsetDateTime,
        modes: &'a BTreeMap<AssetId, PhaseMode>,
    ) -> Self {
        Self {
            valid_from,
            until,
            ev_session: None,
            modes,
        }
    }

    /// Say that a car with a departure time is plugged in.
    #[must_use]
    pub fn with_ev_session(mut self, session: EvStorage) -> Self {
        self.ev_session = Some(session);
        self
    }

    fn mode_of(&self, asset: &Asset) -> PhaseMode {
        self.modes.get(asset.id()).copied().map_or_else(
            || asset.meta().phases.default_mode(),
            |m| asset.meta().phases.clamp_mode(m),
        )
    }
}

/// Every description a site would send, grouped by control type.
#[derive(Debug, Clone, Default)]
pub struct SiteDescription {
    /// The Resource Manager announcement for every asset that has one.
    pub resources: Vec<(AssetId, ResourceManagerDetails)>,
    /// Stores: the battery, and the hot-water tank.
    pub batteries: Vec<(AssetId, BatteryDescription)>,
    /// The charge point, when a car with a departure time is on it.
    pub sessions: Vec<(AssetId, EvDescription)>,
    /// Hot-water tanks.
    pub tanks: Vec<(AssetId, DhwDescription)>,
    /// Power envelopes: the charge point without a deadline, the inverter, a
    /// heat pump that takes a ceiling.
    pub envelopes: Vec<(AssetId, pebc::PowerConstraints)>,
    /// Operation modes: an SG Ready heat pump.
    pub modes: Vec<(AssetId, HeatPumpDescription)>,
    /// Power profiles: the appliances waiting to run one.
    pub programmes: Vec<(AssetId, ProgrammeDescription)>,
    /// Assets the mapping gave a control type and this crate cannot yet express.
    ///
    /// Empty is the goal, and a non-empty entry is a gap that has said so rather
    /// than one that was quietly counted as described.
    pub undescribed: Vec<(AssetId, ControlType)>,
}

impl SiteDescription {
    /// How many of the site's resources were actually described.
    #[must_use]
    pub fn described(&self) -> usize {
        self.batteries.len()
            + self.sessions.len()
            + self.tanks.len()
            + self.envelopes.len()
            + self.modes.len()
            + self.programmes.len()
    }

    /// Every description as the S2 message it would be sent as.
    ///
    /// The point of returning messages rather than the structures: a description
    /// that cannot be serialised is a description that cannot be sent, and a
    /// caller that round-trips these through JSON has checked the whole crate
    /// against the standard's own schema for the price of one assertion.
    #[must_use]
    pub fn messages(&self) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        out.extend(self.resources.iter().map(|(_, r)| r.clone().into()));
        out.extend(self.batteries.iter().map(|(_, d)| d.system.clone().into()));
        out.extend(self.sessions.iter().map(|(_, d)| d.system.clone().into()));
        out.extend(self.tanks.iter().map(|(_, d)| d.system.clone().into()));
        out.extend(self.envelopes.iter().map(|(_, e)| e.clone().into()));
        out.extend(self.modes.iter().map(|(_, d)| d.system.clone().into()));
        out.extend(
            self.programmes
                .iter()
                .map(|(_, d)| d.definition.clone().into()),
        );
        out
    }
}

/// Describe every asset of `site` the way EN 50491-12-2 describes it.
#[must_use]
pub fn describe_site(site: &Site, ctx: &DescribeContext<'_>) -> SiteDescription {
    let mut out = SiteDescription::default();
    for asset in &site.assets {
        let id = asset.id().clone();
        let control = control_type_for(asset, ctx.ev_session.is_some());
        if control == ControlType::NotControllable {
            continue;
        }
        let mode = ctx.mode_of(asset);
        out.resources.push((
            id.clone(),
            resource_manager_details(asset, mode, ctx.ev_session.is_some()),
        ));

        match asset {
            Asset::Battery(b) => out
                .batteries
                .push((id, describe_battery(b, ctx.valid_from))),
            Asset::Dhw(t) => out.tanks.push((id, describe_dhw(t, ctx.valid_from))),
            Asset::Pv(pv) => out.envelopes.push((id, describe_pv(pv, ctx.valid_from))),
            // The same wallbox, described two ways, and that is the whole
            // argument for S2: with a car on it that has a departure time it is
            // a store with a level and a rate, and without one a bound is all a
            // manager can usefully say.
            Asset::Evse(evse) => match &ctx.ev_session {
                Some(session) => out
                    .sessions
                    .push((id, describe_ev(evse, session, mode, ctx.valid_from))),
                None => out
                    .envelopes
                    .push((id, describe_evse(evse, mode, ctx.valid_from))),
            },
            Asset::HeatPump(hp) => match control {
                ControlType::Ombc => out.modes.push((
                    id,
                    describe_heat_pump(hp, site.grid.import_ceiling(), ctx.valid_from),
                )),
                _ => out
                    .envelopes
                    .push((id, heat_pump_envelope(hp, mode, ctx.valid_from))),
            },
            Asset::Load(load) => match load.programme() {
                Some(programme) => out.programmes.push((
                    id,
                    describe_programme(load, programme, ctx.valid_from, ctx.until),
                )),
                None => out.undescribed.push((id, control)),
            },
            Asset::Relay(_) | Asset::Meter(_) => out.undescribed.push((id, control)),
        }
    }
    out
}

/// A heat pump that takes a power ceiling, as a `PEBC` envelope.
///
/// Its consequence is [`Defer`]: a heat pump held down catches up later out of
/// the building's own inertia, which is the whole reason the planner is allowed
/// to hold it down. An inverter's consequence is `Vanish` and the difference is
/// what a manager needs in order to know which of the two to curtail first.
///
/// [`Defer`]: pebc::PowerEnvelopeConsequenceType::Defer
fn heat_pump_envelope(
    hp: &hems_core::asset::HeatPump,
    mode: PhaseMode,
    valid_from: OffsetDateTime,
) -> pebc::PowerConstraints {
    let quantity = match hp.meta.phases.clamp_mode(mode) {
        PhaseMode::Single => s2energy::common::CommodityQuantity::ElectricPowerL1,
        PhaseMode::Three => s2energy::common::CommodityQuantity::ElectricPower3PhaseSymmetric,
    };
    pebc::PowerConstraints::builder()
        .message_id(s2energy::common::Id::generate())
        .id(crate::describe::stable_id(
            &hp.meta.id,
            "heat-pump/envelope",
        ))
        .valid_from(chrono::DateTime::from_timestamp_nanos(
            i64::try_from(valid_from.unix_timestamp_nanos()).unwrap_or(i64::MAX),
        ))
        .consequence_type(pebc::PowerEnvelopeConsequenceType::Defer)
        .allowed_limit_ranges(vec![pebc::AllowedLimitRange {
            commodity_quantity: quantity,
            limit_type: pebc::PowerEnvelopeLimitType::UpperLimit,
            range_boundary: s2energy::common::NumberRange {
                start_of_range: 0.0,
                // The heating rod counts: it is the part of a heat pump a § 14a
                // reduction actually reaches, and a manager told the compressor
                // rating will plan a ceiling the unit can breach on its own.
                end_of_range: hp.meta.connection_power.get(),
            },
            abnormal_condition_only: false,
        }])
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::asset::{
        AssetMeta, Battery, Chemistry, DhwTank, Evse, FlexibleLoad, HeatPump, HeatPumpControl,
        LoadKind, Programme, PvArray,
    };
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-01-15 06:00 UTC);

    fn meta(id: &str, kw: f64) -> AssetMeta {
        AssetMeta::new(
            AssetId::new(id).unwrap(),
            CircuitId::new("main").unwrap(),
            PhaseConnection::Three,
            Power::from_kw(kw),
        )
    }

    fn site() -> Site {
        Site::new(
            SiteId::new(),
            GeoPoint {
                latitude: 52.52,
                longitude: 13.4,
                altitude_m: 34.0,
            },
            GridConnection::new(Current::new(35.0)),
            Circuits::new(vec![Circuit::new(
                CircuitId::new("main").unwrap(),
                None,
                Current::new(35.0),
            )])
            .unwrap(),
            vec![
                Asset::Pv(PvArray {
                    meta: meta("pv", 9.8),
                    kwp_dc: Power::from_kw(9.8),
                    ac_nominal: Power::from_kw(8.0),
                    tilt_deg: 35.0,
                    azimuth_deg: 180.0,
                    para9: Para9Status::default(),
                }),
                Asset::Battery(Battery {
                    meta: meta("battery", 5.0),
                    capacity: Energy::from_kwh(10.0),
                    max_charge: Power::from_kw(5.0),
                    max_discharge: Power::from_kw(5.0),
                    efficiency_charge: 0.95,
                    efficiency_discharge: 0.95,
                    soc_min: Soc::new(0.05).unwrap(),
                    soc_max: Soc::FULL,
                    reserve_soc: Soc::EMPTY,
                    chemistry: Chemistry::Lfp,
                    grid_charging_allowed: true,
                }),
                Asset::Evse(Evse {
                    meta: meta("wallbox", 11.0),
                    min_current: Current::new(6.0),
                    max_current: Current::new(16.0),
                    bidirectional: false,
                    public: false,
                    charge_limit: None,
                }),
                Asset::HeatPump(HeatPump {
                    meta: meta("waermepumpe", 8.0),
                    electrical_nominal: Power::from_kw(5.0),
                    heating_rod: Some(Power::from_kw(3.0)),
                    control: HeatPumpControl::PowerCeiling,
                    modulating: true,
                }),
                Asset::Dhw(DhwTank {
                    meta: meta("warmwasser", 0.5),
                    volume_l: 300.0,
                    heater: Power::from_kw(0.5),
                    cop: 3.0,
                    standing_loss: Power::new(45.0),
                    t_min_c: 45.0,
                    t_set_c: 55.0,
                    t_max_c: 60.0,
                }),
                Asset::Load(FlexibleLoad {
                    meta: meta("spuelmaschine", 2.2),
                    nominal: Power::from_kw(2.0),
                    kind: LoadKind::Shiftable(Programme::uniform(Power::from_kw(1.0), 6)),
                }),
            ],
        )
        .unwrap()
    }

    fn described() -> SiteDescription {
        let modes = BTreeMap::new();
        describe_site(
            &site(),
            &DescribeContext::new(T0, T0 + time::Duration::hours(12), &modes),
        )
    }

    #[test]
    fn every_controllable_asset_of_a_household_is_described() {
        let d = described();
        assert_eq!(d.batteries.len(), 1);
        assert_eq!(d.tanks.len(), 1, "the tank had no description until now");
        assert_eq!(d.programmes.len(), 1, "nor did the dishwasher");
        // Inverter, charge point and the ceiling-controlled heat pump.
        assert_eq!(d.envelopes.len(), 3);
        assert_eq!(d.described(), 6);
        assert!(
            d.undescribed.is_empty(),
            "nothing claimed a control type it cannot express: {:?}",
            d.undescribed
        );
    }

    #[test]
    fn the_same_wallbox_is_an_envelope_empty_and_a_store_with_a_car_on_it() {
        // The whole argument for S2, as an assertion. A manager that has never
        // heard of a car plans this one with the code it plans a battery with —
        // and the moment the cable comes out, the same hardware is a bound.
        let modes = BTreeMap::new();
        let empty = describe_site(
            &site(),
            &DescribeContext::new(T0, T0 + time::Duration::hours(12), &modes),
        );
        assert!(empty.sessions.is_empty());
        assert_eq!(empty.envelopes.len(), 3);

        let plugged = describe_site(
            &site(),
            &DescribeContext::new(T0, T0 + time::Duration::hours(12), &modes).with_ev_session(
                crate::EvStorage {
                    stored: Energy::from_kwh(18.0),
                    capacity: Energy::from_kwh(60.0),
                    efficiency: 0.92,
                },
            ),
        );
        assert_eq!(plugged.sessions.len(), 1);
        assert_eq!(plugged.envelopes.len(), 2);
        assert_eq!(plugged.described(), 6);
        assert!(plugged.undescribed.is_empty());

        // A charge point below 6 A is idle, and the description says so: a
        // manager handed a range starting at zero will ask for 2 kW on three
        // conductors and believe a car is charging.
        let element = &plugged.sessions[0].1.system.actuators[0].operation_modes[0].elements[0];
        assert!(element.power_ranges[0].start_of_range > 4_000.0);
    }

    #[test]
    fn every_description_survives_the_standards_own_wire_format() {
        // A description that cannot be serialised is a description that cannot
        // be sent. `s2energy` is generated from the official schema, so parsing
        // one back is the whole crate checked against the standard.
        //
        // Compared by `message_type` and not by value: an S2 fill rate is a
        // double, and a battery's is 1,319 4 × 10⁻⁶ kWh/s — a number whose
        // shortest decimal form does not survive a round trip in its last unit
        // in the last place. Asserting bitwise equality would be asserting
        // something about `serde_json`'s float printer rather than about this
        // crate.
        let messages = described().messages();
        assert!(!messages.is_empty(), "a household with nothing to say");
        for message in messages {
            let json = serde_json::to_string(&message).expect("serialises");
            let back: Message = serde_json::from_str(&json).expect("round-trips");
            assert_eq!(
                std::mem::discriminant(&back),
                std::mem::discriminant(&message),
                "{json}"
            );
            assert!(
                json.contains("message_type"),
                "the wire form names itself: {json}"
            );
        }
    }

    #[test]
    fn identifiers_do_not_change_between_two_descriptions_of_the_same_site() {
        // An instruction names an operation mode by ID. A Resource Manager that
        // re-mints them on reconnect invalidates every description a manager
        // cached and every instruction still in flight.
        let (a, b) = (described(), described());
        assert_eq!(a.batteries[0].1.charge, b.batteries[0].1.charge);
        assert_eq!(a.tanks[0].1.heat, b.tanks[0].1.heat);
        assert_eq!(a.programmes[0].1.sequence, b.programmes[0].1.sequence);
        assert_eq!(a.envelopes[0].1.id, b.envelopes[0].1.id);
    }
}
