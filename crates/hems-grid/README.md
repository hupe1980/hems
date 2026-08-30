# hems-grid

The German grid rules for a home energy system — **executable, cited and tested**.

Every rule names its source. `[A1 4.5.2]` is Anlage 1 to BNetzA decision
BK6-22-300; `[LPC-031]` is the EEBUS *Limitation of Power Consumption* use-case
specification. A rule without a citation is a bug.

| What | Where it comes from |
|---|---|
| Which devices are steuerbare Verbrauchseinrichtungen, and the per-Fallgruppe summation | `[A1 2.4]` |
| Participation and the transitional regimes to 2028 | `[A1 3.1]`, `[A1 10]` |
| The minimum power a network operator may not go below, with the Gleichzeitigkeitsfaktor table | `[A1 4.5]` |
| The five-state EEBUS LPC/LPP machine: heartbeat, failsafe, release | EEBUS LPC TS §§ 2.2–2.3 |
| The § 9 EEG 60 % cap, what lifts it, and the EEBUS feed-in factor | § 9 Abs. 1–2 EEG (Solarspitzengesetz), `[MGCP-011]` |
| Modul 3 HT/NT/ST windows and their validity rules | BDEW AWH Modul 3 V1.1 |
| The two-year evidence record, with every commanded ceiling | `[A1 7]` |
| Which kilowatt-hour through a battery was green, quarter by quarter | MiSpeL Anlage 1 (1)–(33), Anlage 2 (P1)–(P15) |
| Allocating an energy-sharing community's generation | § 42c EnWG Abs. 1, 3, 4 |

📍 **Every limit is measured where its rule measures it.** § 14a bounds the
netzwirksamer Leistungsbezug — what the controllable devices draw *from the
grid* — so photovoltaic surplus and a discharging battery both raise the ceiling.
§ 9 EEG bounds what leaves the *connection point*, so the household's own
consumption is headroom. `site_feed_in_ceiling` is the one call a daemon needs
for the second of those.

🤝 **Where the rule belongs to [`metering`](https://github.com/hupe1980/metering),
this crate calls it.** `P_min,14a`, the netzwirksamer Leistungsbezug and its
`Verursachungsregel`, the Modul 3 Zählzeitdefinition and its conformance rules,
and the allocation identity § 42b and § 42c settle on are `metering`'s.
`hems-grid` contributes what is specific to *control*: grouping a site's assets
into the devices the Festlegung counts, the transitional regimes, the LPC state
machine, the evidence record, MiSpeL, and the § 42c cascade.

🧊 No I/O, no clock, no async — the state machine is sans-I/O, so a week of
Steuerbox behaviour is a unit test in microseconds.

💶 Control is `f64`, settlement is `rust_decimal::Decimal`. The MiSpeL and § 42c
modules compute quantities that become money and a Nachweis, so they are exact
and the type system keeps them apart from the optimiser's view of the same site.

```rust
use hems_grid::para14a::{ControlMode, SteuVe, minimum_power};
use hems_core::prelude::{AssetId, Fallgruppe, Power};

let devices = [
    SteuVe { assets: vec![AssetId::new("wallbox")?], fallgruppe: Fallgruppe::Ladepunkt, power: Power::from_kw(11.0) },
    SteuVe { assets: vec![AssetId::new("battery")?], fallgruppe: Fallgruppe::Stromspeicher, power: Power::from_kw(5.0) },
];
// 4,2 kW + (2 − 1) × 0,8 × 4,2 kW = 7,56 kW
assert!((minimum_power(&devices, ControlMode::Ems).kw() - 7.56).abs() < 1e-9);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## License

MIT OR Apache-2.0
