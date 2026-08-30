//! The electrical tree between the grid connection and each asset.
//!
//! A house is not a single busbar. A wallbox in the garage sits behind a
//! sub-distribution board with its own cable and its own fuse, and the sum of
//! everything behind that board is bounded by it — independently of, and
//! usually well below, the main connection. Load management that only knows the
//! main fuse either trips the sub-board or leaves capacity unused.
//!
//! Circuits form a tree rooted at the grid connection. The arbiter narrows every
//! asset's feasible interval by every limit on its path to the root, which is
//! the same mechanism the § 14a and § 9 EEG limits use — they are simply limits
//! that sit at the root.

use crate::error::SiteError;
use crate::ids::{AssetId, CircuitId};
use crate::units::{Current, Power};

/// One node of the electrical tree.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Circuit {
    /// The name used in configuration.
    pub id: CircuitId,
    /// The circuit this one hangs off. `None` for the root.
    #[cfg_attr(feature = "serde", serde(default))]
    pub parent: Option<CircuitId>,
    /// A human label for the UI.
    #[cfg_attr(feature = "serde", serde(default))]
    pub label: String,
    /// The fuse rating per outer conductor.
    #[cfg_attr(feature = "serde", serde(default))]
    pub fuse_current: Option<Current>,
    /// A power ceiling that is not simply the fuse — a cable rating, or a limit
    /// the operator wants for their own reasons.
    #[cfg_attr(feature = "serde", serde(default))]
    pub power_limit: Option<Power>,
}

impl Circuit {
    /// A circuit with a fuse rating.
    #[must_use]
    pub fn new(id: CircuitId, parent: Option<CircuitId>, fuse_current: Current) -> Self {
        Self {
            label: id.to_string(),
            id,
            parent,
            fuse_current: Some(fuse_current),
            power_limit: None,
        }
    }

    /// The tightest power ceiling this circuit imposes on a symmetric
    /// three-phase draw, given the nominal voltage.
    ///
    /// A single-phase asset is bounded by the *per-phase* current instead; the
    /// arbiter uses [`Circuit::fuse_current`] directly for that case, because
    /// collapsing a per-phase limit into a total is exactly the mistake that
    /// lets one conductor overload while the total looks fine.
    #[must_use]
    pub fn symmetric_power_limit(&self, voltage: crate::units::Voltage) -> Option<Power> {
        let from_fuse = self.fuse_current.map(|i| i.to_power_3p(voltage));
        match (from_fuse, self.power_limit) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// The circuit tree of one site, with the lookups the arbiter needs.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Circuits {
    circuits: Vec<Circuit>,
}

impl Circuits {
    /// Build from a list, checking that the result is a tree.
    ///
    /// # Errors
    /// [`SiteError::DuplicateId`] for a repeated name,
    /// [`SiteError::UnknownParent`] for a dangling parent,
    /// [`SiteError::CircuitCycle`] when the parent links form a cycle.
    pub fn new(circuits: Vec<Circuit>) -> Result<Self, SiteError> {
        for (i, c) in circuits.iter().enumerate() {
            if circuits[..i].iter().any(|o| o.id == c.id) {
                return Err(SiteError::DuplicateId {
                    kind: "circuit",
                    id: c.id.to_string(),
                });
            }
        }
        for c in &circuits {
            if let Some(parent) = &c.parent
                && !circuits.iter().any(|o| &o.id == parent)
            {
                return Err(SiteError::UnknownParent {
                    circuit: c.id.to_string(),
                    parent: parent.to_string(),
                });
            }
        }
        let this = Self { circuits };
        // Walk upwards from every node; a tree of n nodes has no path longer
        // than n, so exceeding that means the links close a cycle.
        for c in &this.circuits {
            let mut cursor = c;
            for _ in 0..=this.circuits.len() {
                match cursor.parent.as_ref().and_then(|p| this.get(p)) {
                    Some(parent) => cursor = parent,
                    None => break,
                }
                if cursor.id == c.id {
                    return Err(SiteError::CircuitCycle {
                        circuit: c.id.to_string(),
                    });
                }
            }
        }
        Ok(this)
    }

    /// One circuit by name.
    #[must_use]
    pub fn get(&self, id: &CircuitId) -> Option<&Circuit> {
        self.circuits.iter().find(|c| &c.id == id)
    }

    /// Every circuit.
    #[must_use]
    pub fn all(&self) -> &[Circuit] {
        &self.circuits
    }

    /// The circuits from `id` up to the root, innermost first.
    ///
    /// Every limit on this path binds the asset, so the arbiter intersects them.
    #[must_use]
    pub fn path_to_root(&self, id: &CircuitId) -> Vec<&Circuit> {
        let mut path = Vec::new();
        let mut cursor = self.get(id);
        // The constructor rules out cycles, so this terminates; the bound is
        // belt and braces for a `Circuits` built by hand in a test.
        for _ in 0..=self.circuits.len() {
            let Some(c) = cursor else { break };
            path.push(c);
            cursor = c.parent.as_ref().and_then(|p| self.get(p));
        }
        path
    }

    /// The circuits that lie between `asset`'s circuit and the root.
    #[must_use]
    pub fn path_for_asset(&self, asset: &crate::asset::Asset) -> Vec<&Circuit> {
        self.path_to_root(&asset.meta().circuit)
    }

    /// The assets, out of `assets`, that hang off `circuit` or anything below it.
    #[must_use]
    pub fn assets_below<'a>(
        &self,
        circuit: &CircuitId,
        assets: &'a [crate::asset::Asset],
    ) -> Vec<&'a AssetId> {
        assets
            .iter()
            .filter(|a| {
                self.path_to_root(&a.meta().circuit)
                    .iter()
                    .any(|c| &c.id == circuit)
            })
            .map(crate::asset::Asset::id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(s: &str) -> CircuitId {
        CircuitId::new(s).unwrap()
    }

    fn tree() -> Circuits {
        Circuits::new(vec![
            Circuit::new(cid("main"), None, Current::new(63.0)),
            Circuit::new(cid("garage"), Some(cid("main")), Current::new(20.0)),
        ])
        .unwrap()
    }

    #[test]
    fn a_path_collects_every_limit_up_to_the_root() {
        let ids: Vec<_> = tree()
            .path_to_root(&cid("garage"))
            .iter()
            .map(|c| c.id.to_string())
            .collect();
        assert_eq!(ids, ["garage", "main"]);
    }

    #[test]
    fn a_dangling_parent_is_refused() {
        let err = Circuits::new(vec![Circuit::new(
            cid("garage"),
            Some(cid("nope")),
            Current::new(20.0),
        )])
        .unwrap_err();
        assert!(matches!(err, SiteError::UnknownParent { .. }));
    }

    #[test]
    fn a_cycle_is_refused_rather_than_hanging_the_arbiter() {
        let err = Circuits::new(vec![
            Circuit::new(cid("a"), Some(cid("b")), Current::new(20.0)),
            Circuit::new(cid("b"), Some(cid("a")), Current::new(20.0)),
        ])
        .unwrap_err();
        assert!(matches!(err, SiteError::CircuitCycle { .. }));
    }

    #[test]
    fn a_duplicate_name_is_refused() {
        let err = Circuits::new(vec![
            Circuit::new(cid("main"), None, Current::new(63.0)),
            Circuit::new(cid("main"), None, Current::new(35.0)),
        ])
        .unwrap_err();
        assert!(matches!(
            err,
            SiteError::DuplicateId {
                kind: "circuit",
                ..
            }
        ));
    }

    #[test]
    fn a_fuse_becomes_a_symmetric_power_ceiling() {
        let c = Circuit::new(cid("garage"), None, Current::new(20.0));
        // 3 × 20 A × 230 V = 13,8 kW
        let limit = c
            .symmetric_power_limit(crate::units::NOMINAL_VOLTAGE)
            .unwrap();
        assert!((limit.kw() - 13.8).abs() < 1e-9);
    }
}
