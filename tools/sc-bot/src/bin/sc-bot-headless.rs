//! Launches the real client headless and drives it through its test-command pipes. UNVERIFIED against a live client: see
//! docs/BOT.md, "Headless client". Nothing here names a machine: the client command, its arguments and the Game.log come from
//! the command line or `SC_BOT_*` variables.

use std::{path::PathBuf, process::{Command, ExitCode, Stdio}, time::Duration};

use clap::Parser;
use sc_bot::headless::*;
use serde_json::json;

#[derive(Parser)]
#[command(about = "Runs the real client headless through its test-command pipes (unverified against a live client)")]
struct Args {
    /// The client command (a wrapper such as a Wine or Proton launcher is part of it).
    #[arg(long, env = "SC_BOT_CLIENT")]
    client: String,
    /// An argument for the client, repeated. The headless and login flags are added unless `--no-default-args`.
    #[arg(long = "client-arg", allow_hyphen_values = true)]
    client_args: Vec<String>,
    #[arg(long)]
    no_default_args: bool,
    /// Directory for the two FIFOs the client's pipe arguments name.
    #[arg(long, env = "SC_BOT_PIPE_DIR")]
    pipe_dir: PathBuf,
    /// The client runs under Wine: name the FIFOs as `Z:\...` paths.
    #[arg(long)]
    wine: bool,
    /// Prefix of the `testing_pipe_in` / `testing_pipe_out` arguments; the notes did not check it.
    #[arg(long, default_value = "-")]
    arg_prefix: String,
    /// The client's Game.log, tailed from its size at launch.
    #[arg(long, env = "SC_BOT_GAME_LOG")]
    game_log: PathBuf,
    /// A console command to run once in game, repeated.
    #[arg(long)]
    console: Vec<String>,
    /// `x,y,z`: go there with `map goto_global` once the client is in a level.
    #[arg(long)]
    goto: Option<String>,
    /// `map_name,home_name`: send `map connect` when the client is at the front end.
    #[arg(long)]
    connect: Option<String>,
    /// Text the log must contain after the run, repeated.
    #[arg(long)]
    expect_log: Vec<String>,
    /// Seconds to wait for the client's `connected`.
    #[arg(long, default_value_t = 300)]
    connect_timeout: u64,
    /// Seconds to wait for a state-gated command to be accepted.
    #[arg(long, default_value_t = 300)]
    state_timeout: u64,
    /// Seconds to let the client run after the last command, before the log is read.
    #[arg(long, default_value_t = 20)]
    settle: u64,
    #[arg(long)]
    report: Option<PathBuf>,
}

fn step(steps: &mut Vec<Step>, name: &str, answer: Option<(i64, serde_json::Value)>) {
    let code = answer.map(|a| a.0);
    eprintln!("{name}: {code:?}");
    steps.push(Step { name: name.to_string(), ok: code == Some(result::OK), result: code });
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let fail = |text: String| {
        eprintln!("sc-bot-headless: {text}");
        ExitCode::from(2)
    };
    if let Err(e) = std::fs::create_dir_all(&args.pipe_dir) {
        return fail(format!("{}: {e}", args.pipe_dir.display()));
    }
    let (pipe_in, pipe_out) = (args.pipe_dir.join("test_in"), args.pipe_dir.join("test_out"));
    for pipe in [&pipe_in, &pipe_out] {
        if let Err(e) = make_fifo(pipe) {
            return fail(e.to_string());
        }
    }
    let name = |path: &PathBuf| if args.wine { windows_path(path) } else { path.display().to_string() };
    let log_offset = std::fs::metadata(&args.game_log).map(|m| m.len()).unwrap_or(0);

    let mut command = Command::new(&args.client);
    if !args.no_default_args {
        command.args(DEFAULT_CLIENT_ARGS);
    }
    command.args(&args.client_args);
    command.arg(format!("{}testing_pipe_in", args.arg_prefix)).arg(name(&pipe_in));
    command.arg(format!("{}testing_pipe_out", args.arg_prefix)).arg(name(&pipe_out));
    command.stdin(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return fail(format!("start {}: {e}", args.client)),
    };

    // The harness writes the client's input and reads its output (the client is the pipe client).
    let (writer, reader) = match (open_fifo(&pipe_in).await, open_fifo(&pipe_out).await) {
        (Ok(writer), Ok(reader)) => (writer, reader),
        (Err(e), _) | (_, Err(e)) => return fail(format!("open fifo: {e}")),
    };
    let mut harness = Harness::new(reader, writer);
    let mut steps = Vec::new();
    let connected = harness.wait_connected(Duration::from_secs(args.connect_timeout)).await.unwrap_or(false);
    steps.push(Step { name: "client connected to the pipes".into(), result: None, ok: connected });
    if connected {
        let (wait, retry) = (Duration::from_secs(args.state_timeout), Duration::from_secs(2));
        if let Some(target) = &args.connect {
            let (map, home) = target.split_once(',').unwrap_or((target, ""));
            step(&mut steps, "map connect", harness.command_when_ready("map", "connect", json!({"map_name": map, "home_name": home}), wait, retry).await.ok().flatten());
        }
        if let Some(position) = &args.goto {
            let n: Vec<f64> = position.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            if let [x, y, z] = n[..] {
                step(&mut steps, "map goto_global", harness.command_when_ready("map", "goto_global", json!({"position": {"x": x, "y": y, "z": z}}), wait, retry).await.ok().flatten());
            } else {
                steps.push(Step { name: "map goto_global (bad --goto)".into(), result: None, ok: false });
            }
        }
        for text in &args.console {
            step(&mut steps, &format!("console {text}"), harness.console(text, Duration::from_secs(30)).await.ok().flatten());
        }
    }
    tokio::time::sleep(Duration::from_secs(args.settle)).await;
    let _ = child.kill();
    let _ = child.wait();

    let (log_findings, log_required) = match scan_log(&args.game_log, log_offset, &default_forbidden(), &args.expect_log) {
        Ok(found) => found,
        Err(e) => return fail(format!("{}: {e}", args.game_log.display())),
    };
    let passed = connected && steps.iter().all(|s| s.ok) && log_findings.is_empty() && log_required.values().all(|f| *f);
    let report = HeadlessReport { unverified: UNVERIFIED, connected, steps, log_findings, log_required, passed };
    let json = serde_json::to_string_pretty(&report).unwrap_or_default();
    match &args.report {
        Some(path) => {
            let _ = std::fs::write(path, json);
        }
        None => println!("{json}"),
    }
    ExitCode::from(if passed { 0 } else { 1 })
}
