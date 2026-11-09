//! sc-bot command line. Everything that names a place or an account comes from arguments or `SC_BOT_*` variables.

use std::{path::PathBuf, process::ExitCode, time::Duration};

use clap::Parser;
use sc_bot::{
    hub::LoginData,
    scenario::{self, Options},
    scenarios,
    session::Target,
};

fn number(text: &str) -> Result<u64, String> {
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    }
    .map_err(|e| format!("{text}: {e}"))
}

fn class(text: &str) -> Result<u32, String> {
    u32::try_from(number(text)?).map_err(|e| e.to_string())
}

#[derive(Parser)]
#[command(version, about = "Headless protocol-level test bot for the shard: joins as a game client and checks what the servers send back")]
struct Args {
    /// A `LoginData.json` of the client files: the account token, and the Hub address when it names one.
    #[arg(long, env = "SC_BOT_LOGIN_DATA")]
    login_data: Option<PathBuf>,
    /// The account token, in place of `--login-data`.
    #[arg(long, env = "SC_BOT_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// The Hub, `host:port`.
    #[arg(long, env = "SC_BOT_HUB")]
    hub: Option<String>,
    /// `client.crt`, the certificate authority the Hub's certificate chains to.
    #[arg(long, env = "SC_BOT_CA")]
    ca: Option<PathBuf>,
    /// The location id to go to; 0 for the Hub's default.
    #[arg(long, env = "SC_BOT_LOCATION", default_value = "0", value_parser = number)]
    location: u64,
    /// The shard's UDP address `host:port`, when it is not the one the Hub names.
    #[arg(long, env = "SC_BOT_SHARD")]
    shard: Option<String>,
    /// A session id to connect with, in place of a Hub login (needs `--shard`).
    #[arg(long, env = "SC_BOT_SESSION")]
    session: Option<String>,
    /// `http://host:port` of a DGS's gRPC, to check its player count after a disconnect.
    #[arg(long, env = "SC_BOT_DGS_URL")]
    dgs_url: Option<String>,
    /// The player's method index of the inventory process method.
    #[arg(long, env = "SC_BOT_PROCESS_METHOD", default_value_t = sc_bot::requests::DEFAULT_PROCESS_METHOD)]
    process_method: u32,
    /// The location id of the player's item bank (`<player>:Location:<id>`).
    #[arg(long, env = "SC_BOT_BANK_LOCATION", value_parser = number)]
    bank_location: Option<u64>,
    /// Class CRC of a helmet stowed in the bank.
    #[arg(long, env = "SC_BOT_HELMET_CLASS", default_value = "0x431a7340", value_parser = class)]
    helmet_class: u32,
    #[arg(long, env = "SC_BOT_HELMET_PORT", default_value = "Body_ItemPort:Armor_Undersuit:Armor_Helmet")]
    helmet_port: String,
    /// Class CRC of a stackable item stowed in the bank (two or more).
    #[arg(long, env = "SC_BOT_STACK_CLASS", value_parser = class)]
    stack_class: Option<u32>,
    /// Class CRC of a stack of wearable items in the bank.
    #[arg(long, env = "SC_BOT_STACK_EQUIP_CLASS", value_parser = class)]
    stack_equip_class: Option<u32>,
    #[arg(long, env = "SC_BOT_STACK_EQUIP_PORT", default_value = "Body_ItemPort:Armor_Undersuit:medPen_attach_1")]
    stack_equip_port: String,
    /// An entity of another player, to check that its inventory access token is denied.
    #[arg(long, env = "SC_BOT_FOREIGN_ITEM", value_parser = number)]
    foreign_item: Option<u64>,
    /// The join must bind the hangar's ATC, kiosks and call panels (a home whose hangar has the ASOP kiosks' container).
    #[arg(long)]
    expect_atc: bool,
    /// Seconds to wait for a carriage after the call-panel press; 0 does not wait.
    #[arg(long, env = "SC_BOT_ELEVATOR_WAIT", default_value_t = 45)]
    elevator_wait: u64,
    /// Seconds a request may wait for its answer.
    #[arg(long, default_value_t = 10)]
    reply_timeout: u64,
    /// Seconds the join may take.
    #[arg(long, default_value_t = 60)]
    join_timeout: u64,
    /// Seconds one scenario may take in all.
    #[arg(long, default_value_t = 240)]
    scenario_timeout: u64,
    /// The scenarios to run, by name, separated by commas; all by default.
    #[arg(long, value_delimiter = ',')]
    scenario: Vec<String>,
    /// A skipped scenario counts as a failure.
    #[arg(long)]
    strict: bool,
    /// Writes the JSON report here; it goes to stdout otherwise.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Lists the scenarios and exits.
    #[arg(long)]
    list: bool,
}

fn usage(message: &str) -> ExitCode {
    eprintln!("sc-bot: {message}");
    ExitCode::from(2)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if args.list {
        for scenario in scenarios::ALL {
            println!("{:<22} {}", scenario.name, scenario.about);
        }
        return ExitCode::SUCCESS;
    }
    let mut selected = Vec::new();
    if args.scenario.is_empty() {
        selected.extend(scenarios::ALL.iter());
    }
    for name in &args.scenario {
        match scenarios::find(name) {
            Some(scenario) => selected.push(scenario),
            None => return usage(&format!("no scenario '{name}' (see --list)")),
        }
    }
    let login = match &args.login_data {
        Some(path) => match LoginData::read(path) {
            Ok(login) => Some(login),
            Err(error) => return usage(&error),
        },
        None => None,
    };
    let target = Target {
        hub: args.hub.or_else(|| login.as_ref().and_then(|login| login.hub.clone())),
        ca: args.ca,
        token: args.token.or_else(|| login.map(|login| login.token)),
        location: args.location,
        shard: args.shard,
        session: args.session,
        join_timeout: Duration::from_secs(args.join_timeout),
    };
    if target.session.is_none() && (target.hub.is_none() || target.ca.is_none() || target.token.is_none()) {
        return usage("needs a Hub login (--login-data or --token, --hub, --ca) or a --session with --shard");
    }
    let opts = Options {
        target,
        dgs_url: args.dgs_url,
        process_method: args.process_method,
        bank_location: args.bank_location,
        helmet_class: args.helmet_class,
        helmet_port: args.helmet_port,
        stack_class: args.stack_class,
        stack_equip_class: args.stack_equip_class,
        stack_equip_port: args.stack_equip_port,
        foreign_item: args.foreign_item,
        expect_atc: args.expect_atc,
        elevator_wait: Duration::from_secs(args.elevator_wait),
        reply_timeout: Duration::from_secs(args.reply_timeout),
        strict: args.strict,
    };
    let report = scenario::run(&opts, &selected, Duration::from_secs(args.scenario_timeout)).await;
    let json = serde_json::to_string_pretty(&report).unwrap_or_default();
    match &args.report {
        Some(path) => {
            if let Err(error) = std::fs::write(path, json) {
                return usage(&format!("write {}: {error}", path.display()));
            }
        }
        None => println!("{json}"),
    }
    eprintln!("{} passed, {} failed, {} skipped, {} errors", report.passed, report.failed, report.skipped, report.errors);
    ExitCode::from(report.exit_code(opts.strict))
}
