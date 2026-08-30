//! One installation: the grid connection, the circuits, the assets.

use metering::{MaloId, MeloId};

use crate::asset::Asset;
use crate::circuit::Circuits;
use crate::error::SiteError;
use crate::ids::{AssetId, SiteId};
use crate::units::{Current, Power};

/// Where the site is, for the solar geometry and the weather forecast.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GeoPoint {
    /// Degrees north.
    pub latitude: f64,
    /// Degrees east.
    pub longitude: f64,
    /// Metres above sea level.
    #[cfg_attr(feature = "serde", serde(default))]
    pub altitude_m: f64,
}

/// The Netzanschlusspunkt — where the installation meets the public grid.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GridConnection {
    /// The market location, when the site is registered in the German market.
    #[cfg_attr(feature = "serde", serde(default))]
    pub malo: Option<MaloId>,
    /// The metering location.
    #[cfg_attr(feature = "serde", serde(default))]
    pub melo: Option<MeloId>,
    /// The network operator's BDEW code number, as it appears on the § 14a
    /// agreement and in the market communication.
    #[cfg_attr(feature = "serde", serde(default))]
    pub dso_code: Option<String>,
    /// The Netzbereich the operator has assigned the connection to.
    ///
    /// `[BK6-22-300 A1 8.2.b]` requires the operator to tell the customer which
    /// one it is, and `[A1 8.4]` requires a monthly machine-readable list of
    /// control actions per area. Knowing the area is what lets the planner
    /// anticipate where and when reductions cluster.
    #[cfg_attr(feature = "serde", serde(default))]
    pub netzbereich: Option<String>,
    /// The main fuse rating per outer conductor.
    pub fuse_current: Current,
    /// The contractually agreed connection power, where one is agreed.
    ///
    /// This is the value a CEM reports as `ContractualConsumptionNominalMax`
    /// in the EEBUS LPC use case (`[LPC-042]`).
    #[cfg_attr(feature = "serde", serde(default))]
    pub contract_power: Option<Power>,
}

impl GridConnection {
    /// A connection with nothing but a fuse — enough to run a site.
    #[must_use]
    pub fn new(fuse_current: Current) -> Self {
        Self {
            malo: None,
            melo: None,
            dso_code: None,
            netzbereich: None,
            fuse_current,
            contract_power: None,
        }
    }

    /// The largest symmetric import the connection permits: the smaller of the
    /// fuse rating and any contractual limit.
    #[must_use]
    pub fn import_ceiling(&self) -> Power {
        let from_fuse = self.fuse_current.to_power_3p(crate::units::NOMINAL_VOLTAGE);
        match self.contract_power {
            Some(contract) => from_fuse.min(contract),
            None => from_fuse,
        }
    }

    /// The largest symmetric export the connection permits, as a positive
    /// magnitude.
    ///
    /// The fuse alone. [`GridConnection::contract_power`] is deliberately not
    /// applied here: it is the *ContractualConsumptionNominalMax* of `[LPC-042]`
    /// — an agreement about how much the household may **draw** — and a system
    /// whose feed-in is limited is limited by § 9 EEG, an LPP session or the
    /// Einspeisezusage, none of which is this number. Applying a consumption
    /// agreement to production would curtail a roof for a limit nobody wrote.
    #[must_use]
    pub fn export_ceiling(&self) -> Power {
        self.fuse_current.to_power_3p(crate::units::NOMINAL_VOLTAGE)
    }
}

/// One installation.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Site {
    /// Fleet-unique identity.
    pub id: SiteId,
    /// A human label.
    #[cfg_attr(feature = "serde", serde(default))]
    pub label: String,
    /// Where it is.
    pub location: GeoPoint,
    /// The grid connection.
    pub grid: GridConnection,
    /// The electrical tree.
    pub circuits: Circuits,
    /// Everything behind the connection.
    pub assets: Vec<Asset>,
}

impl Site {
    /// Build a site and check that it is internally consistent.
    ///
    /// # Errors
    /// [`SiteError`] for a duplicate asset name, an asset on an unknown circuit,
    /// or a circuit tree that is not a tree.
    pub fn new(
        id: SiteId,
        location: GeoPoint,
        grid: GridConnection,
        circuits: Circuits,
        assets: Vec<Asset>,
    ) -> Result<Self, SiteError> {
        let site = Self {
            id,
            label: String::new(),
            location,
            grid,
            circuits,
            assets,
        };
        site.validate()?;
        Ok(site)
    }

    /// Check the cross-references.
    ///
    /// # Errors
    /// [`SiteError`] as described on [`Site::new`].
    pub fn validate(&self) -> Result<(), SiteError> {
        for (i, a) in self.assets.iter().enumerate() {
            if self.assets[..i].iter().any(|o| o.id() == a.id()) {
                return Err(SiteError::DuplicateId {
                    kind: "asset",
                    id: a.id().to_string(),
                });
            }
            if self.circuits.get(&a.meta().circuit).is_none() {
                return Err(SiteError::UnknownCircuit {
                    asset: a.id().to_string(),
                    circuit: a.meta().circuit.to_string(),
                });
            }
        }
        Ok(())
    }

    /// One asset by name.
    #[must_use]
    pub fn asset(&self, id: &AssetId) -> Option<&Asset> {
        self.assets.iter().find(|a| a.id() == id)
    }

    /// The assets that are meters of the grid connection point.
    pub fn grid_meters(&self) -> impl Iterator<Item = &Asset> {
        self.assets.iter().filter(
            |a| matches!(a, Asset::Meter(m) if m.role == crate::asset::MeterRole::GridConnection),
        )
    }

    /// How far the measured grid power is from the sum of the measured assets.
    ///
    /// With the load convention of [`crate::units`], the site balance is
    ///
    /// ```text
    /// grid == Σ assets
    /// ```
    ///
    /// so a residual that is not near zero means a meter is missing, mis-signed
    /// or stale. `hems-realtime` watches it, and a large residual makes the
    /// arbiter fall back to conservative assumptions rather than optimise
    /// against a fiction.
    #[must_use]
    pub fn balance_residual(grid: Power, assets: impl IntoIterator<Item = Power>) -> Power {
        grid - assets.into_iter().sum::<Power>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AssetMeta, CapRelief, Capabilities, Evse, FlexibleLoad, LoadKind, PvArray};
    use crate::circuit::Circuit;
    use crate::ids::CircuitId;
    use crate::units::PhaseConnection;

    fn cid(s: &str) -> CircuitId {
        CircuitId::new(s).unwrap()
    }

    fn meta(id: &str, circuit: &str, kw: f64) -> AssetMeta {
        AssetMeta::new(
            AssetId::new(id).unwrap(),
            cid(circuit),
            PhaseConnection::Three,
            Power::from_kw(kw),
        )
        .with_capabilities(Capabilities::MEASURE)
    }

    fn site() -> Site {
        Site::new(
            SiteId::new(),
            GeoPoint {
                latitude: 52.52,
                longitude: 13.40,
                altitude_m: 34.0,
            },
            GridConnection::new(Current::new(35.0)),
            Circuits::new(vec![
                Circuit::new(cid("main"), None, Current::new(35.0)),
                Circuit::new(cid("garage"), Some(cid("main")), Current::new(20.0)),
            ])
            .unwrap(),
            vec![
                Asset::Pv(PvArray {
                    meta: meta("pv", "main", 9.8),
                    kwp_dc: Power::from_kw(9.8),
                    ac_nominal: Power::from_kw(8.0),
                    tilt_deg: 35.0,
                    azimuth_deg: 180.0,
                    cap_relief: CapRelief::None,
                }),
                Asset::Evse(Evse {
                    meta: meta("wallbox", "garage", 11.0),
                    min_current: Current::new(6.0),
                    max_current: Current::new(16.0),
                    bidirectional: false,
                    public: false,
                }),
                Asset::Load(FlexibleLoad {
                    meta: meta("haushalt", "main", 3.0),
                    nominal: Power::from_kw(0.5),
                    kind: LoadKind::Fixed,
                }),
            ],
        )
        .unwrap()
    }

    #[test]
    fn an_asset_on_an_unknown_circuit_is_refused() {
        let mut s = site();
        s.assets.push(Asset::Load(FlexibleLoad {
            meta: meta("pool", "keller", 1.0),
            nominal: Power::from_kw(1.0),
            kind: LoadKind::Interruptible,
        }));
        assert!(matches!(
            s.validate(),
            Err(SiteError::UnknownCircuit { .. })
        ));
    }

    #[test]
    fn a_duplicate_asset_name_is_refused() {
        let mut s = site();
        s.assets.push(Asset::Load(FlexibleLoad {
            meta: meta("pv", "main", 1.0),
            nominal: Power::from_kw(1.0),
            kind: LoadKind::Fixed,
        }));
        assert!(matches!(
            s.validate(),
            Err(SiteError::DuplicateId { kind: "asset", .. })
        ));
    }

    #[test]
    fn the_balance_closes_when_the_signs_are_right() {
        // PV producing 5 kW, household drawing 1 kW, wallbox charging 3 kW.
        let pv = Power::from_kw(-5.0);
        let haus = Power::from_kw(1.0);
        let wallbox = Power::from_kw(3.0);
        // Net: 1 + 3 − 5 = −1 kW, i.e. exporting a kilowatt.
        let grid = Power::from_kw(-1.0);
        assert!(Site::balance_residual(grid, [pv, haus, wallbox]).abs() < Power::new(1.0));
    }

    #[test]
    fn the_import_ceiling_takes_the_stricter_of_fuse_and_contract() {
        let mut g = GridConnection::new(Current::new(63.0));
        assert!((g.import_ceiling().kw() - 43.47).abs() < 0.01);
        g.contract_power = Some(Power::from_kw(30.0));
        assert_eq!(g.import_ceiling(), Power::from_kw(30.0));
    }

    #[test]
    fn assets_below_a_circuit_are_found_through_the_tree() {
        let s = site();
        let below = s.circuits.assets_below(&cid("garage"), &s.assets);
        assert_eq!(below.len(), 1);
        assert_eq!(below[0].as_str(), "wallbox");
        assert_eq!(s.circuits.assets_below(&cid("main"), &s.assets).len(), 3);
    }
}
