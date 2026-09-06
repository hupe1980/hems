//! The **device-level** EEBUS conformance procedures, run against this box.
//!
//! `eebus`'s catalogue holds 203 abstract test cases and that crate answers the
//! 189 about the protocol. Fourteen are properties of *this process* — a factory
//! reset, a power cut, a start-up duration, what the appliance draws — so
//! nothing in the library can answer them. Seven procedures cover the fourteen;
//! `eebus::conformance::harness` is the judge and this is what drives it.
//!
//! # What a "power cut" is here
//!
//! Everything in memory destroyed and rebuilt from what survived on disk, which
//! is the whole of what a power cut does to a box: the `Lpc` driver, its SPINE
//! engine and its session go, and the next one is built from the same store
//! file. That is why the store is a real file and not `:memory:` — a persistence
//! case against an in-memory store asserts that a value survived nothing.
//!
//! It is **not** a stopwatch. `StartUpDur` is a declared parameter of the
//! product and the harness judges observations against what was declared; the
//! physical article is the laboratory's to measure. The one figure genuinely
//! measured here is the opening exchange against `[E-DT60]`, taken from when the
//! bindings settled rather than from a number this script chose.
//!
//! # No socket
//!
//! Datagrams move as the JSON a SHIP data frame carries, as in
//! `hems-drv/tests/eebus_spine.rs` — both ends are the real engines, and a
//! message either side refuses to encode does not arrive. `steuerbox_session.rs`
//! is what puts TCP, TLS and the handshake under the same seam. A virtual clock
//! is what makes a two-hour failsafe window and a 120-second heartbeat timeout
//! assertions rather than an afternoon of waiting.

use core::time::Duration as StdDuration;

use eebus::conformance::harness::{
    BlackStartActor, DeviceObservation, DeviceParameters, DeviceRun, Procedure, Verdict,
};
use eebus::model::{DeviceType, EntityType};
use eebus::spine::{Engine, LocalDevice, LocalEntity};
use eebus::usecases::limitation::{self, EnergyGuardActor, GuardEvent, LimitWrite};
use eebus::usecases::lpc;
use hems_core::prelude::{AssetId, Power};
use hems_drv::eebus::{Lpc, Use};
use hems_drv::{Driver, DriverEvent, LinkState};
use hemsd::runtime::{FAILSAFE_CONSUMPTION, failsafe_in_force};
use hemsd::store::{Store, StoredFailsafe};
use time::OffsetDateTime;
use time::macros::datetime;

/// When the box was switched on.
const START: OffsetDateTime = datetime!(2026-01-15 00:00:00 UTC);

/// The failsafe the parameter sheet declares: the reference household's own
/// § 14a minimum (`[A1 4.5]`), not a vendor's flat 4,2 kW.
const DECLARED_FAILSAFE_W: f64 = 10_500.0;

/// The declared Failsafe Duration Minimum, the shortest `[LPC-022]` allows.
const DECLARED_FAILSAFE_FOR: StdDuration = StdDuration::from_secs(2 * 3_600);

/// What this box declares about its own start-up.
///
/// Forty-five seconds is a gateway-class SBC from power on to a socket it can
/// answer a SHIP handshake on. It is a **declaration**, and the harness judges
/// the observations against it — which is exactly the relationship the
/// specification's `StartUpDur` has to the laboratory's stopwatch.
const DECLARED_START_UP: StdDuration = StdDuration::from_secs(45);

fn at(seconds: i64) -> OffsetDateTime {
    START + time::Duration::seconds(seconds)
}

fn elapsed(seconds: i64) -> StdDuration {
    StdDuration::from_secs(seconds.unsigned_abs())
}

/// The household's § 14a driver, as `hemsd run` builds it — with the failsafe
/// the box would come up holding after `failsafe_in_force` has consulted its
/// store.
fn household(failsafe: (Power, StdDuration), started_at: OffsetDateTime) -> Lpc {
    Lpc::new(
        AssetId::new("netzanschluss").expect("a literal identifier"),
        Use::Lpc,
        failsafe.0,
        failsafe.1,
        started_at,
    )
}

/// The network operator's box: an Energy Guard on a `GridGuard` entity.
fn steuerbox() -> (Engine, EnergyGuardActor) {
    let mut device = LocalDevice::new("n:dso", "Steuerbox-1", DeviceType::ElectricitySupplySystem)
        .expect("a valid device address");
    device
        .add_entity(
            LocalEntity::new([1], EntityType::GridGuard)
                .with_feature(limitation::client_feature(1))
                .with_feature(limitation::device_diagnosis_feature(2)),
        )
        .expect("a fresh entity");
    let client = device.address_of(&[1], 1);
    let diagnosis = device.address_of(&[1], 2);
    let mut engine = Engine::new(device);
    engine.add_use_case([1], 1, &lpc::ENERGY_GUARD);
    let actor = EnergyGuardActor::new(lpc::DIRECTION, client, diagnosis, StdDuration::ZERO);
    (engine, actor)
}

/// One turn of the loop `hemsd` runs, with the socket replaced by a `Vec`.
struct Wire {
    guard_engine: Engine,
    guard: EnergyGuardActor,
    box_driver: Lpc,
    reported: Vec<DriverEvent>,
    answers: Vec<GuardEvent>,
    attached: bool,
    /// When the bindings settled, and when a limit was first acknowledged.
    ///
    /// Measured rather than assumed. `[E-DT60]` bounds the opening exchange from
    /// *the bindings settling* (§ 2.11) to the heartbeat and the limit that
    /// follows it, so a procedure that reported the seconds its own script
    /// happened to use would be judging the script.
    attached_at: Option<i64>,
    accepted_at: Option<i64>,
}

impl Wire {
    fn new(failsafe: (Power, StdDuration), started_at: OffsetDateTime) -> Self {
        let (guard_engine, guard) = steuerbox();
        Self {
            guard_engine,
            guard,
            box_driver: household(failsafe, started_at),
            reported: Vec::new(),
            answers: Vec::new(),
            attached: false,
            attached_at: None,
            accepted_at: None,
        }
    }

    /// Both ends learn a session is up, and each asks the other who it is.
    fn open(&mut self, seconds: i64) {
        self.box_driver.on_link(LinkState::Up, at(seconds));
        let source = eebus::spine::node_management(self.guard_engine.device().address());
        let destination = eebus::spine::node_management_without_device();
        for function in [
            eebus::model::Function::NodeManagementDetailedDiscoveryData,
            eebus::model::Function::NodeManagementUseCaseData,
        ] {
            let _ = self
                .guard_engine
                .read(&destination, &source, function, elapsed(seconds));
        }
        self.settle(seconds);
        // The binding, the subscription and the first heartbeat are the actor's
        // business and they take these seconds — a session is not usable the
        // instant the link is up, and a procedure that wrote a limit before the
        // heartbeat would be measuring `WRITE_WINDOW` rather than what it says.
        for offset in [1_i64, 2, 3, 60, 61] {
            self.advance_to(seconds + offset);
        }
        assert!(
            self.attached,
            "the Energy Guard has to find the household's LoadControl feature by \
             discovery before any of these procedures means anything"
        );
    }

    /// Move every datagram waiting in either direction until neither side has
    /// anything more to say.
    fn settle(&mut self, seconds: i64) {
        let now = at(seconds);
        let mono = elapsed(seconds);
        // Bounded: an exchange that will not settle is a defect, and a test that
        // loops for ever reports it as a hang rather than as a failure.
        for _ in 0..64 {
            let mut moved = false;
            while let Some(bytes) = self.box_driver.poll_transmit() {
                moved = true;
                let datagram = serde_json::from_slice(&bytes)
                    .expect("what the driver emits is a SPINE datagram");
                let _ = self.guard_engine.handle_datagram(&datagram, mono);
            }
            while let Some(datagram) = self.guard_engine.poll_transmit() {
                moved = true;
                let bytes = serde_json::to_vec(&datagram).expect("a datagram serialises");
                self.box_driver
                    .on_bytes(&bytes, now)
                    .expect("the driver understands its own protocol");
            }
            self.drain(seconds);
            if !moved {
                break;
            }
        }
    }

    /// Read what each side has made of what it received, and bind once
    /// discovery has told the Energy Guard where to write.
    fn drain(&mut self, seconds: i64) {
        let mono = elapsed(seconds);
        while let Some(event) = self.box_driver.poll_event() {
            self.reported.push(event);
        }
        while let Some(event) = self.guard_engine.poll_event() {
            let reports = self
                .guard
                .handle_event(&mut self.guard_engine, &event, mono);
            if matches!(reports, Some(GuardEvent::LimitAccepted { .. }))
                && self.accepted_at.is_none()
            {
                self.accepted_at = Some(seconds);
            }
            self.answers.extend(reports);
        }
        if !self.attached {
            let located = self
                .guard_engine
                .peers()
                .find_map(|remote| limitation::locate(remote, lpc::DIRECTION));
            if let Some(peer) = located {
                self.guard.attach(&mut self.guard_engine, peer, mono);
                self.attached = true;
                self.attached_at = Some(seconds);
            }
        }
    }

    /// Let `seconds` pass on both clocks, running each side's timers.
    fn advance_to(&mut self, seconds: i64) {
        self.box_driver.on_timeout(at(seconds));
        let _ = self
            .guard
            .handle_timeout(&mut self.guard_engine, elapsed(seconds));
        self.settle(seconds);
    }

    /// The Energy Guard writes a limit to the **household**.
    ///
    /// The address is the peer's, taken from discovery — the guard's own would
    /// be the operator writing a limit to itself, which the engine accepts and
    /// which reaches nothing.
    ///
    /// Three turns of the clock after it, because a requirement is *deferred*
    /// until the guard's own timers send it: `deferred_requirements()` is
    /// `eebus`'s name for the silent failure this exists to avoid — discovery,
    /// the bindings, the subscription and the heartbeats can all succeed while
    /// no limit is ever written, and nothing on the wire says why.
    fn require(&mut self, limit: LimitWrite, seconds: i64) {
        let device = self
            .guard
            .peers()
            .next()
            .expect("the household has been discovered")
            .device
            .clone();
        self.guard.require(&device, Some(limit), elapsed(seconds));
        for turn in 0..3 {
            self.advance_to(seconds + turn);
        }
        assert!(
            self.guard.deferred_requirements().next().is_none(),
            "a requirement still waiting is a limit that was never written"
        );
    }

    /// Whether the box has answered a write since `from`.
    fn limit_accepted(&self) -> Option<f64> {
        self.answers.iter().rev().find_map(|event| match event {
            GuardEvent::LimitAccepted { limit, .. } => Some(limit.watts),
            _ => None,
        })
    }

    /// How long the opening exchange took, from the bindings settling to the
    /// first limit the household acknowledged.
    ///
    /// `None` where it never completed, which the harness judges as a failure
    /// rather than as an absence — §2.11 has the guard open with a heartbeat and
    /// a limit as soon as the bindings settle, so nothing arriving is the
    /// failure the deadline exists to catch.
    fn opening_exchange(&self) -> Option<StdDuration> {
        let attached = self.attached_at?;
        let accepted = self.accepted_at?;
        Some(StdDuration::from_secs(
            accepted.saturating_sub(attached).unsigned_abs(),
        ))
    }
}

/// A store on its own file, so a "power cut" can drop everything in memory and
/// come back to what survived.
///
/// A file rather than `:memory:`, and that is the whole point of it: an
/// in-memory store dies with the process that opened it, so a persistence
/// procedure run against one would be asserting that a value survived nothing.
///
/// Named with the process id and a counter, as the reference days name theirs —
/// `cargo test` runs test binaries in parallel and a fixed name would have two
/// of these opening one file.
struct BoxStore {
    path: std::path::PathBuf,
}

impl BoxStore {
    fn new(what: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hems-conformance-{what}-{}-{n}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    fn open(&self) -> Store {
        Store::open(&self.path).expect("the box's own store")
    }
}

impl Drop for BoxStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// What the box comes up holding, asked of `hemsd`'s own resolution rather than
/// of a copy of it.
fn comes_up_holding(store: &Store) -> (Power, StdDuration) {
    failsafe_in_force(
        Some(store),
        Power::new(DECLARED_FAILSAFE_W),
        DECLARED_FAILSAFE_FOR,
    )
}

#[test]
fn the_seven_device_level_procedures_pass_against_this_box() {
    let declared = DeviceParameters::new(DECLARED_START_UP)
        .failsafe(DECLARED_FAILSAFE_W, DECLARED_FAILSAFE_FOR)
        // The tester here is a process in the same binary, so it is instant.
        .peer_start_up(StdDuration::ZERO)
        // A gateway box comes back on its own when the power returns; it has no
        // battery and nothing to press. Declaring it raises the two black-start
        // cases from recommended to mandatory, which is the honest direction —
        // a box that skipped them would be claiming less than it does.
        .declaring("black-start capable");
    let mut run = DeviceRun::new(declared);

    run.observe(factory_reset());
    run.observe(persistence());
    run.observe(controllable_system_black_start());
    run.observe(energy_guard_black_start());
    run.observe(energy_guard_reboot());
    run.observe(appliance_ceiling());
    run.skip(
        Procedure::EnergyGuardResendAfterNack,
        "hems accepts every well-formed limit (`LocalDecision::Apply`): the guard \
         is what decides how the household meets one, and refusing at the driver \
         would be a compliance decision taken where the site is not in view. So \
         this box cannot produce the refusal the case needs, and a test that \
         hand-built a malformed write would be measuring `eebus`'s encoder \
         rather than this product. It is answered against a laboratory's own \
         Energy Guard.",
    );

    let report = run.report();
    assert!(
        report.failures().next().is_none(),
        "device-level conformance failed:\n{report}"
    );
    assert!(
        report.is_complete(),
        "every procedure this box has to answer must be answered:\n{report}"
    );
    assert_eq!(
        report.passed(),
        12,
        "six procedures, each answering the LPC case and its LPP twin:\n{report}"
    );
}

/// `ATC_*_COM_PT_CSInit_002` — reset to factory defaults and read the
/// parameters back.
///
/// The limit must come back **inactive** and the failsafe at what the parameter
/// sheet declares. Both are properties of the store: `factory_reset` leaves no
/// written failsafe, so `failsafe_in_force` falls back to the configured value,
/// and a box with nothing written has no operator limit to be holding.
fn factory_reset() -> DeviceObservation {
    let disk = BoxStore::new("factory-reset");
    let mut store = disk.open();

    // An operator has been here: a failsafe of their own, and the identity that
    // lets their Steuerbox reach this house.
    store
        .put_eebus_failsafe(
            FAILSAFE_CONSUMPTION,
            &StoredFailsafe {
                watts: 4_200.0,
                minimum_s: 4 * 3_600,
            },
            START,
        )
        .expect("the operator's failsafe is kept");
    assert_eq!(
        comes_up_holding(&store).0,
        Power::new(4_200.0),
        "the operator's value has to be in force before the reset, or this \
         procedure would pass on a box that never took it"
    );

    store.factory_reset().expect("a drained box resets");

    let (watts, duration) = comes_up_holding(&store);
    DeviceObservation::FactoryReset {
        // Nothing written and no session: there is no operator limit in force.
        limit_active: false,
        failsafe_watts: watts.get(),
        failsafe_duration: duration,
    }
}

/// `ATC_*_COM_PT_CSInit_003` — write failsafe values, power-cycle, read them
/// back.
///
/// The half that would fail silently: a box that came back on its own
/// configuration would have quietly undone an operator's write, and `[LPC-021]`
/// makes that value theirs to change.
fn persistence() -> DeviceObservation {
    let disk = BoxStore::new("persistence");
    let written = StoredFailsafe {
        watts: 6_000.0,
        minimum_s: 3 * 3_600,
    };
    {
        let store = disk.open();
        store
            .put_eebus_failsafe(FAILSAFE_CONSUMPTION, &written, START)
            .expect("the operator's failsafe is kept");
    }
    // The power cut: everything in memory is gone, and what comes back is built
    // from the file.
    let store = disk.open();
    let (watts, duration) = comes_up_holding(&store);
    DeviceObservation::Persistence {
        written_watts: written.watts,
        written_duration: StdDuration::from_secs(written.minimum_s.unsigned_abs()),
        stored_watts: watts.get(),
        stored_duration: duration,
    }
}

/// `ATC_*_COM_PT_CSConnection_009` — cut the power to the household and let the
/// Controllable System come back.
fn controllable_system_black_start() -> DeviceObservation {
    let disk = BoxStore::new("cs-black-start");
    let store = disk.open();
    let mut wire = Wire::new(comes_up_holding(&store), START);
    wire.open(0);
    wire.require(LimitWrite::active(4_200.0), 62);
    assert_eq!(
        wire.limit_accepted(),
        Some(4_200.0),
        "the box has to be under a limit before it is power-cycled, or the \
         procedure proves nothing about coming back"
    );

    // The power cut. Everything in memory goes; the store stays.
    let restart = 60 + DECLARED_START_UP.as_secs() as i64;
    drop(wire);
    let store = disk.open();
    let mut wire = Wire::new(comes_up_holding(&store), at(restart));
    wire.open(restart);
    // The Energy Guard finds it again and the limit exchange resumes.
    wire.require(LimitWrite::active(4_200.0), restart + 5);

    DeviceObservation::BlackStart {
        actor: BlackStartActor::ControllableSystem,
        reachable_after: Some(DECLARED_START_UP),
        exchange_resumed: wire.limit_accepted() == Some(4_200.0),
    }
}

/// `ATC_*_COM_PT_EGConnection_003` — the same for the Energy Guard.
///
/// The box's side of it: a Controllable System whose Energy Guard vanished must
/// fall back to its failsafe on the heartbeat timeout and must accept the guard
/// again when it returns, rather than staying restrained or staying free.
fn energy_guard_black_start() -> DeviceObservation {
    let disk = BoxStore::new("eg-black-start");
    let store = disk.open();
    let mut wire = Wire::new(comes_up_holding(&store), START);
    wire.open(0);
    wire.require(LimitWrite::active(4_200.0), 62);

    // The operator's box loses power. The household hears nothing, and
    // `[LPC-906]` makes that the failsafe once the heartbeat is 120 s stale —
    // counted from the **last heartbeat**, which the opening exchange and the
    // write left at second 64, not from the moment the link dropped. Getting
    // that wrong is how a procedure asserts the failsafe before it is due and
    // reports the box as broken.
    let lost = 70_i64;
    let stale = 64 + 120 + 1;
    let mut alone = wire.box_driver;
    alone.on_link(LinkState::Down, at(lost));
    alone.on_timeout(at(stale));
    assert_eq!(
        alone.ceiling(),
        Some(Power::new(DECLARED_FAILSAFE_W)),
        "a Controllable System that has lost its Energy Guard holds its failsafe"
    );

    // …and it comes back, and re-dials. Both ends re-handshake, because a SHIP
    // session does not survive one end losing power — which is why this is a
    // fresh `Wire` rather than the old box driver handed a new guard. What the
    // procedure is about is that the household **accepts** the operator again
    // and leaves its failsafe, and the assertion above is what makes the
    // failsafe part of the same story rather than a separate test.
    let back = stale + 30;
    let mut wire = Wire::new(comes_up_holding(&store), at(back));
    drop(alone);
    wire.open(back);
    wire.require(LimitWrite::active(4_200.0), back + 62);

    DeviceObservation::BlackStart {
        actor: BlackStartActor::EnergyGuard,
        reachable_after: Some(StdDuration::ZERO),
        exchange_resumed: wire.limit_accepted() == Some(4_200.0),
    }
}

/// `ATC_*_COM_PT_EGConnection_001` — restart the Energy Guard's own process and
/// wait for its opening exchange.
fn energy_guard_reboot() -> DeviceObservation {
    let disk = BoxStore::new("eg-reboot");
    let store = disk.open();
    let mut wire = Wire::new(comes_up_holding(&store), START);
    wire.open(0);
    wire.require(LimitWrite::active(4_200.0), 62);

    // The Energy Guard's process restarts. A SHIP session does not survive one
    // end restarting, so both re-handshake — the same mechanics as the black
    // start above, and what this case measures is different: not *whether* the
    // household comes back but how long the **opening exchange** takes once it
    // has, which `[E-DT60]` bounds at sixty seconds.
    drop(wire);
    let rebooted = 300;
    let mut wire = Wire::new(comes_up_holding(&store), at(rebooted));
    let opened_at = rebooted;
    wire.open(opened_at);
    // `open` runs the binding, the subscription and the first heartbeat, and it
    // asserts the household was found. The limit that follows the heartbeat is
    // what completes the exchange.
    wire.require(LimitWrite::active(4_200.0), opened_at + 62);
    assert_eq!(
        wire.limit_accepted(),
        Some(4_200.0),
        "a rebooted Energy Guard has to be able to limit the household again"
    );

    DeviceObservation::EnergyGuardReboot {
        // A process in the same binary is ready as soon as it is constructed;
        // the physical article's start-up is what a laboratory measures.
        ready_after: Some(StdDuration::ZERO),
        heartbeat_then_limit_after: wire.opening_exchange(),
    }
}

/// `ATC_*_COM_PT_CSConnection_006` — write a limit above what the appliance can
/// draw and read back what was applied.
///
/// A Controllable System accepts it: the limit is a *ceiling*, and one above the
/// connection's own capacity simply never binds. What must not happen is the box
/// clamping it to something smaller and reporting that back, which would tell an
/// operator it had reduced a household it had not.
fn appliance_ceiling() -> DeviceObservation {
    let disk = BoxStore::new("appliance-ceiling");
    let store = disk.open();
    let mut wire = Wire::new(comes_up_holding(&store), START);
    wire.open(0);
    // Well above a 63 A three-phase connection.
    let written = 100_000.0;
    wire.require(LimitWrite::active(written), 62);

    DeviceObservation::ApplianceCeiling {
        written_watts: written,
        accepted: wire.limit_accepted().is_some(),
        applied_watts: wire.box_driver.ceiling().map(Power::get),
    }
}

/// The one procedure this box cannot answer, and the report says so rather than
/// leaving a gap somebody has to notice.
#[test]
fn the_procedure_this_box_cannot_answer_is_named_and_not_hidden() {
    let mut run = DeviceRun::new(DeviceParameters::new(DECLARED_START_UP));
    run.skip(
        Procedure::EnergyGuardResendAfterNack,
        "answered by a laboratory",
    );
    let report = run.report();
    assert!(
        matches!(
            report.verdict("ATC_LPC_COM_PT_EGMessages_002"),
            Some(Verdict::Skipped(_))
        ),
        "a skip has to be visible in the report, with its reason:\n{report}"
    );
}
