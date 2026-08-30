# hems-realtime

The guard plane and the one-second control loop of
[hems](https://github.com/hupe1980/hems).

**Nothing the arbiter does can widen what the guard decided.** That is not a
convention — the guard runs first and turns every limit into an interval, the
arbiter only clamps into it, and a randomised property test over households,
measurements, plans and user overrides asserts the § 14a promise on every tick.

- 🛡️ **Per-asset limits** — the hardware's own asymmetric ratings, state of
  charge, the backup reserve, every fuse on the path to the connection, feed-in.
- 🤝 **Shared limits, shared properly** — the § 14a ceiling (spent only by the
  controllable devices, and lifted by photovoltaic surplus *and* by a discharging
  store), the grid connection in **both** directions and each sub-distribution
  board (spent by *everything* behind them), the § 9 EEG cap on what leaves the
  connection point, and the 4,6 kVA of unbalanced load VDE-AR-N 4100 allows. Each
  goes through one weighted max-min allocator and the results intersect. Bounding
  each asset by a shared limit individually is the mistake that looks right: 11 kW
  of wallbox, 8 kW of heat pump and 5 kW of battery each fit under a 24 kW
  connection and burn it together — and a roof allowed 5,88 kW beside a battery
  allowed 5,88 kW put 11,76 kW through a limit of 5,88. The 4,6 kVA reaches
  fewer devices than it looks like it should: VDE-AR-N 4100 covers only
  equipment that can feed in or store, so a kettle on L1 is outside it.
- ⚖️ **Where the Festlegung leaves a choice, it is a setting.** `[A1 2.3]`
  defines the netzwirksamer Leistungsbezug and does not say how to split it when
  a roof is producing, so `GuardConfig::verursachungsregel` names the convention
  — defaulting to the one that can never understate the controllable share.
- 🔋 **A store may lend its discharge to the devices it is feeding.** `[A1 2.3]`
  measures what the controllable devices draw *from the grid*, so a battery
  covering the wallbox is headroom the household owns. The guard lends only what
  is measured, what this tick is about to ask for anyway, and what the store can
  sustain for a whole control period — and pins the lender's ceiling at zero in
  exchange, so the tick that spends a discharge cannot also reverse it.
- 🚿 **An absent instruction is not a zero for every device.** An inverter, a heat
  pump and a hot-water tank answer a request for *less* and have controls of
  their own, so with no plan they run at their maximum power point and their own
  thermostats. Reading silence as "off" — which is right for a battery and a
  charge point — is how a box whose planner stopped let the house go cold.
- 🔇 **A missing measurement is not a zero** — a silent controllable device is
  assumed at its nameplate, a silent inverter at nothing. Guessing high for load
  and low for generation is the only pair of guesses that cannot overstate a
  budget, and the verdict names every asset it had to guess for.
- 🔌 **One conductor or three** — a three-phase charge point cannot start below
  4,14 kW and a single-phase one starts at 1,38 kW, so 2 kW of surplus either
  charges a car or does not, depending on a contactor. The policy has hysteresis
  *relative to the mode* (applied to the threshold instead, it tells a charge
  point already on three conductors to drop to one, where it delivers less), an
  asymmetric confirmation window (failing to switch down stops the session;
  failing to switch up only slows it), and a dwell time.
- 🔋 **A promise is a bound on power, not on state** — "stop discharging once the
  reserve is reached" is already broken by the time it can be checked: at 5 kW
  from a 10 kWh pack, one minute is 0,8 % of capacity. The guard bounds the power
  so a whole control period cannot cross the floor.
- 🧊 **No I/O, no clock** — `now` is a parameter, so a January day with a § 14a
  reduction and a control box that stops talking is a unit test.

## License

MIT OR Apache-2.0
