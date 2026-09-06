//! What `histd` is told.

use std::path::PathBuf;

/// Everything `histd` is configured with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The shared daemon settings.
    #[serde(flatten)]
    pub service: hems_service::Settings,
    /// One bearer token per site — the credential its box presents.
    ///
    /// A box may read and write **its own** record and no other. Each is a
    /// [`hems_service::Secret`], so the usual deployment writes
    /// `haus-1 = "env:HEMS_HISTD_TOKEN_HAUS1"` and no credential enters the
    /// configuration file.
    ///
    /// Empty means this service accepts **nothing**, which is the safe reading
    /// of "nobody configured it": what these routes serve is a household's whole
    /// consumption record and the evidence a network operator settles on.
    #[serde(default)]
    pub site_tokens: std::collections::BTreeMap<String, hems_service::Secret>,
    /// Tokens that may read **any** site's § 14a evidence.
    ///
    /// A network operator checking a reduction it commanded, or an internal
    /// service building a portfolio view. They may not write — an operator that
    /// could write the record of its own control actions is marking its own
    /// homework — and they may not read the Data Act export, which is the
    /// household's under Article 4 and not theirs.
    /// Which households each tenant covers.
    ///
    /// A shared deployment hosting several operators names each one's
    /// households here, and an operator credential names the tenant it belongs
    /// to. A single-tenant deployment leaves this empty and writes
    /// `tenant = "*"` on its credential, which is the same reach stated out
    /// loud rather than arrived at by omission (D112).
    #[serde(default)]
    pub tenants: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// The operator credentials, each scoped to a tenant.
    #[serde(default)]
    pub operators: Vec<hems_service::OperatorCredential>,
    /// Where the database lives.
    ///
    /// `:memory:` is honoured and is what the tests use. A box uses a path on
    /// its own flash, and a fleet deployment points this at a volume — because a
    /// two-year record on an ephemeral filesystem is a two-hour record.
    pub database: PathBuf,
    /// How often to delete what has aged out, seconds.
    ///
    /// Daily. `[A1 7.3]`'s two years are not a number anybody is racing, and a
    /// gateway box has better things to do than sweep a table every minute.
    pub retention_sweep_s: u64,
    /// Which MiSpeL option each site settles under, `[MiSpeL Tenor]`.
    ///
    /// A **declared** fact about the installation, exactly like
    /// `Para9Status`: no meter reading says whether a storage system has ever
    /// been charged from the grid, whether a charge point is bidirectional, or
    /// which of the three options the Anlagenbetreiber chose. The Festlegung
    /// has them choose; this is where the choice is written down, so a Nachweis
    /// is computed under the option the household is actually on rather than
    /// under whichever one the arithmetic happens to support.
    ///
    /// A site that is absent from this map has not declared one, and
    /// `/v1/sites/{site}/mispel` answers `404` rather than guessing — settling
    /// a household under the wrong Basisfall produces a Nachweis that is
    /// arithmetically perfect and about a different installation.
    #[serde(default)]
    pub mispel: std::collections::BTreeMap<String, MispelSettings>,
    /// The Model Context Protocol surface, off by default.
    ///
    /// It holds a household's data, so a token is **required** when it is
    /// switched on and the surface answers as whatever authority that token
    /// already carries here — an operator, or one site.
    #[serde(default)]
    pub mcp: hems_service::McpSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            service: hems_service::Settings::default(),
            site_tokens: std::collections::BTreeMap::new(),
            tenants: std::collections::BTreeMap::new(),
            operators: Vec::new(),
            mispel: std::collections::BTreeMap::new(),
            database: PathBuf::from("hems-history.sqlite"),
            retention_sweep_s: 24 * 3600,
            mcp: hems_service::McpSettings::default(),
        }
    }
}

impl AsMut<hems_service::Settings> for Settings {
    fn as_mut(&mut self) -> &mut hems_service::Settings {
        &mut self.service
    }
}

/// Which MiSpeL option one site settles under, and the facts its arithmetic
/// needs.
///
/// The three options of the Festlegung, and the two that need arithmetic carry
/// the case they are evaluated under — a fact about what is behind the
/// Einspeisestelle rather than about the meter readings, which is why it is
/// declared and not derived.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, tag = "option", rename_all = "kebab-case")]
pub enum MispelSettings {
    /// **Ausschließlichkeit** — the store is never charged from the grid, so no
    /// energy through it is ever grey and there is nothing to separate.
    ///
    /// It settles nothing and is nevertheless **checked**. The claim is a
    /// statement about the registers, and `(1)¼ = MIN[Z1NB¼ ; Z2V¼]` — the
    /// gleichzeitiger Netzbezug — is what measures it: the option holds exactly
    /// while that is zero in every quarter hour of the period. The export
    /// computes it and names the quarter hours where it is not, because the
    /// alternative is a household learning that its levy privilege lapsed from
    /// its network operator rather than from its own box. `[MiSpeL A1 2.1.5]`
    /// for the formula, and D142 for the planner's half — it refuses to
    /// *schedule* a simultaneous charge, and this is what says none happened.
    Ausschliesslichkeit,
    /// **Abgrenzung** (Anlage 1) — the quarter-hourly formulas (1)–(33),
    /// settled per calendar month.
    Abgrenzung {
        /// Which of the four Basisfälle the installation is, `[MiSpeL A1 3]`.
        basisfall: hems_grid::mispel::Basisfall,
    },
    /// **Pauschal** (Anlage 2) — the annual flat lines (P1)–(P15), for solar of
    /// at most 30 kWp, settled per calendar year.
    Pauschal {
        /// Which of the three Pauschalfälle, `[MiSpeL A2 3]`.
        fall: hems_grid::mispel::PauschalFall,
        /// `Pinst` — installed solar power behind the Einspeisestelle, kWp.
        solar_kwp: f64,
        /// `SKinst` — installed storage capacity behind it, kWh.
        ///
        /// It matters more than its size suggests: `(P2)` widens the
        /// indifference band as the store shrinks against the roof, because a
        /// small store leaves less room for the arbitrage the band exists to
        /// keep out.
        storage_kwh: f64,
    },
}
