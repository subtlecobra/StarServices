//! The scenario runner: what a scenario is, how it records checks, and the JSON report with the exit code.

use std::{collections::BTreeMap, future::Future, pin::Pin, time::{Duration, Instant}};

use serde::Serialize;

use crate::{
    requests::DEFAULT_PROCESS_METHOD,
    session::{Session, Target},
};

/// Everything a scenario may need, all from the command line or environment.
#[derive(Clone, Debug)]
pub struct Options {
    pub target: Target,
    /// `http://host:port` of the DGS's gRPC (`DgsControl`), to read its player count; optional.
    pub dgs_url: Option<String>,
    /// The player's method index of `RmAuthorityProcessInventoryManagementOperation`.
    pub process_method: u32,
    /// The location id of the player's item bank, `<player>:Location:<id>` (the class CRC of its `StarMapObject`).
    pub bank_location: Option<u64>,
    /// The class CRC of a helmet stowed in the bank that fits `helmet_port`.
    pub helmet_class: u32,
    pub helmet_port: String,
    /// A stackable item stowed in the bank, with at least two in its stack.
    pub stack_class: Option<u32>,
    /// A stack of wearable items in the bank, equipped one at a time into `stack_equip_port`.
    pub stack_equip_class: Option<u32>,
    pub stack_equip_port: String,
    /// An item of another player, to check that its access token is denied.
    pub foreign_item: Option<u64>,
    /// The join must bind the hangar's ATC, kiosks and call panels (the player's home hangar has the kiosk container).
    pub expect_atc: bool,
    /// How long to wait for a carriage after a call-panel press; 0 skips that check.
    pub elevator_wait: Duration,
    /// How long a request or a call may wait for its answer.
    pub reply_timeout: Duration,
    /// A skipped scenario counts as a failure.
    pub strict: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            target: Target { join_timeout: Duration::from_secs(60), ..Target::default() },
            dgs_url: None,
            process_method: DEFAULT_PROCESS_METHOD,
            bank_location: None,
            helmet_class: 0x431a_7340,
            helmet_port: "Body_ItemPort:Armor_Undersuit:Armor_Helmet".into(),
            stack_class: None,
            stack_equip_class: None,
            stack_equip_port: "Body_ItemPort:Armor_Undersuit:medPen_attach_1".into(),
            foreign_item: None,
            expect_atc: false,
            elevator_wait: Duration::from_secs(45),
            reply_timeout: Duration::from_secs(10),
            strict: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Why a scenario ended early.
#[derive(Debug)]
pub enum Stop {
    /// Its inputs are not configured or not in the world: not run, not failed.
    Skip(String),
    /// The bot could not do it (no connection, a timeout in the join): the stack is not known to be wrong.
    Error(String),
}

impl From<String> for Stop {
    fn from(text: String) -> Self {
        Stop::Error(text)
    }
}

pub type Done = Result<(), Stop>;
pub type Run = for<'a> fn(&'a mut Ctx) -> Pin<Box<dyn Future<Output = Done> + 'a>>;

pub struct Scenario {
    pub name: &'static str,
    pub about: &'static str,
    pub run: Run,
}

/// A scenario's workspace: its options, the checks it made, and facts for the report.
pub struct Ctx {
    pub opts: Options,
    pub checks: Vec<Check>,
    pub facts: BTreeMap<String, String>,
}

impl Ctx {
    /// Records a check and returns whether it held. The scenario goes on.
    pub fn check(&mut self, name: &str, ok: bool, detail: impl Into<String>) -> bool {
        let detail = detail.into();
        eprintln!("  {} {name}{}", if ok { "ok  " } else { "FAIL" }, if detail.is_empty() { String::new() } else { format!(" ({detail})") });
        self.checks.push(Check { name: name.to_string(), ok, detail });
        ok
    }

    pub fn fact(&mut self, name: &str, value: impl ToString) {
        self.facts.insert(name.to_string(), value.to_string());
    }

    /// A new session: login, connect and join to `eCVS_InGame`.
    pub async fn join(&mut self) -> Result<Session, Stop> {
        let started = Instant::now();
        let session = Session::join(&self.opts.target).await?;
        self.fact("join_ms", started.elapsed().as_millis());
        Ok(session)
    }

    pub fn require<T>(&self, value: Option<T>, what: &str) -> Result<T, Stop> {
        value.ok_or_else(|| Stop::Skip(format!("{what} is not configured")))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    Failed,
    Skipped,
    Error,
}

#[derive(Debug, Serialize)]
pub struct ScenarioReport {
    pub name: String,
    pub outcome: Outcome,
    pub duration_ms: u128,
    pub checks: Vec<Check>,
    pub facts: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub tool: &'static str,
    pub version: &'static str,
    pub scenarios: Vec<ScenarioReport>,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errors: usize,
}

impl Report {
    /// 0 when everything passed (and, with `strict`, nothing was skipped), 1 when a check failed, 2 when the bot could not
    /// run a scenario at all.
    pub fn exit_code(&self, strict: bool) -> u8 {
        if self.errors > 0 {
            2
        } else if self.failed > 0 || (strict && self.skipped > 0) {
            1
        } else {
            0
        }
    }
}

/// Runs `scenarios` in order, each with the time it is given, and collects the report.
pub async fn run(opts: &Options, scenarios: &[&Scenario], scenario_timeout: Duration) -> Report {
    let mut reports = Vec::new();
    for scenario in scenarios {
        eprintln!("scenario {}: {}", scenario.name, scenario.about);
        let started = Instant::now();
        let mut ctx = Ctx { opts: opts.clone(), checks: Vec::new(), facts: BTreeMap::new() };
        let result = match tokio::time::timeout(scenario_timeout, (scenario.run)(&mut ctx)).await {
            Ok(result) => result,
            Err(_) => Err(Stop::Error(format!("the scenario took longer than {scenario_timeout:?}"))),
        };
        let failed = ctx.checks.iter().any(|check| !check.ok);
        let (outcome, reason) = match result {
            Ok(()) if failed => (Outcome::Failed, None),
            Ok(()) => (Outcome::Passed, None),
            Err(Stop::Skip(why)) => (Outcome::Skipped, Some(why)),
            // Checks that failed before the bot gave up still count as failures.
            Err(Stop::Error(why)) if failed => (Outcome::Failed, Some(why)),
            Err(Stop::Error(why)) => (Outcome::Error, Some(why)),
        };
        eprintln!("  => {outcome:?}{}", reason.as_deref().map(|why| format!(": {why}")).unwrap_or_default());
        reports.push(ScenarioReport { name: scenario.name.to_string(), outcome, duration_ms: started.elapsed().as_millis(), checks: ctx.checks, facts: ctx.facts, reason });
    }
    let count = |outcome| reports.iter().filter(|report| report.outcome == outcome).count();
    Report {
        tool: "sc-bot",
        version: env!("CARGO_PKG_VERSION"),
        passed: count(Outcome::Passed),
        failed: count(Outcome::Failed),
        skipped: count(Outcome::Skipped),
        errors: count(Outcome::Error),
        scenarios: reports,
    }
}
