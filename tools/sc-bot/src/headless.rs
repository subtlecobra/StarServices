//! Drives the real game client in its headless mode through its test-command pipes. EVERYTHING HERE ABOUT THE LIVE CLIENT IS
//! UNVERIFIED: it follows the RE repository's `headless_and_bot_assessment.md` (addenda 1 and 2), which is static analysis
//! of 4.10.2 and was never run. The framing of what the client sends is proven from its writer (compact JSON plus one NUL);
//! what it accepts is inferred from that (`{...}\0`). Only the framing and message parsing are tested here, against a fake peer.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// `ETestCommandResult`.
pub mod result {
    pub const OK: i64 = 0;
    pub const FAILED: i64 = 1;
    pub const ADAPTER_FAILURE: i64 = 2;
    pub const ADAPTER_NOT_FOUND: i64 = 4;
    pub const NOT_SUPPORTED: i64 = 5;
}

/// One command, as `{...}` and a NUL.
pub fn encode_command(command_id: i64, adapter: &str, command: &str, args: Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&json!({"command_id": command_id, "adapter_name": adapter, "command_name": command, "command_args": args})).unwrap_or_default();
    bytes.push(0);
    bytes
}

/// What the client sends.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    /// `{"type":"TestSystem","name":"connected"}`.
    Connected,
    Event { category: String, name: String, content: Value },
    Response { command_id: i64, result: i64, details: Value },
    /// A message that is JSON but none of these.
    Other(Value),
}

pub fn parse_incoming(bytes: &[u8]) -> Result<Incoming, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| format!("not JSON: {e}"))?;
    let kind = value["type"].as_str().unwrap_or_default();
    if kind == "command_response" {
        return Ok(Incoming::Response { command_id: value["command_id"].as_i64().ok_or("no command_id")?, result: value["result"].as_i64().ok_or("no result")?, details: value["details"].clone() });
    }
    if kind == "TestSystem" && value["name"] == "connected" {
        return Ok(Incoming::Connected);
    }
    if let (false, Some(name)) = (kind.is_empty(), value["name"].as_str()) {
        return Ok(Incoming::Event { category: kind.to_string(), name: name.to_string(), content: value["content"].clone() });
    }
    Ok(Incoming::Other(value))
}

/// Splits a byte stream at NULs.
#[derive(Default)]
pub struct NulFramer {
    buffer: Vec<u8>,
}

impl NulFramer {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub fn next_message(&mut self) -> Option<Vec<u8>> {
        let end = self.buffer.iter().position(|b| *b == 0)?;
        let message: Vec<u8> = self.buffer.drain(..=end).take(end).collect();
        Some(message)
    }
}

/// The harness's end of the two pipes: it reads what the client writes and writes what the client reads.
pub struct Harness<R, W> {
    from_client: R,
    to_client: W,
    framer: NulFramer,
    next_id: i64,
    pub events: Vec<Incoming>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Harness<R, W> {
    pub fn new(from_client: R, to_client: W) -> Self {
        Self { from_client, to_client, framer: NulFramer::default(), next_id: 1, events: Vec::new() }
    }

    /// The next message, or `None` when `wait` passes first.
    pub async fn receive(&mut self, wait: Duration) -> io::Result<Option<Incoming>> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(message) = self.framer.next_message() {
                let parsed = parse_incoming(&message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                self.events.push(parsed.clone());
                return Ok(Some(parsed));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let mut chunk = [0u8; 1024];
            match tokio::time::timeout(left, self.from_client.read(&mut chunk)).await {
                Err(_) => return Ok(None),
                Ok(Ok(0)) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the client closed its pipe")),
                Ok(Ok(n)) => self.framer.push(&chunk[..n]),
                Ok(Err(e)) => return Err(e),
            }
        }
    }

    /// Waits for `{"type":"TestSystem","name":"connected"}`.
    pub async fn wait_connected(&mut self, wait: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.receive(left).await? {
                Some(Incoming::Connected) => return Ok(true),
                Some(_) => {}
                None => return Ok(false),
            }
        }
    }

    /// Sends a command and waits for its response: `(result, details)`; `None` on timeout.
    pub async fn command(&mut self, adapter: &str, command: &str, args: Value, wait: Duration) -> io::Result<Option<(i64, Value)>> {
        let id = self.next_id;
        self.next_id += 1;
        self.to_client.write_all(&encode_command(id, adapter, command, args)).await?;
        self.to_client.flush().await?;
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.receive(left).await? {
                Some(Incoming::Response { command_id, result, details }) if command_id == id => return Ok(Some((result, details))),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    /// Runs a command that is only valid in some state, again until it is accepted (result 0) or `wait` passes: the headless
    /// manager answers 1 in a state that does not take it.
    pub async fn command_when_ready(&mut self, adapter: &str, command: &str, args: Value, wait: Duration, retry: Duration) -> io::Result<Option<(i64, Value)>> {
        let deadline = Instant::now() + wait;
        loop {
            let answer = self.command(adapter, command, args.clone(), retry.max(Duration::from_millis(1))).await?;
            match answer {
                Some((result::OK, details)) => return Ok(Some((result::OK, details))),
                Some((code, details)) if Instant::now() >= deadline || code != result::FAILED => return Ok(Some((code, details))),
                None if Instant::now() >= deadline => return Ok(None),
                _ => tokio::time::sleep(retry).await,
            }
        }
    }

    pub async fn console(&mut self, text: &str, wait: Duration) -> io::Result<Option<(i64, Value)>> {
        self.command("console", "command", json!({"value": text}), wait).await
    }
}

/// What a line of Game.log must not contain. The wording of the first three is from lines seen in real logs
/// (docs/INVENTORY.md, docs/SHIPS.md); the rest are guesses and may match nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    Contains(String),
    /// `closing inventory with N pending moves` with N above 0.
    PendingMoves,
}

pub fn default_forbidden() -> Vec<(&'static str, Rule)> {
    vec![
        ("pending inventory moves at close", Rule::PendingMoves),
        ("kiosk without an ATC", Rule::Contains("[NO ATC]".into())),
        ("snapshot for an unbound entity", Rule::Contains("Snapshot Update For Missing Entity".into())),
        ("failed inventory request (guess)", Rule::Contains("Result[Failed]".into())),
        ("item type mismatch", Rule::Contains("Entity Item Types Mismatch".into())),
        ("rollback of a local attachment (guess)", Rule::Contains("Expire Local Attachment".into())),
    ]
}

impl Rule {
    pub fn matches(&self, line: &str) -> bool {
        match self {
            Rule::Contains(text) => line.contains(text.as_str()),
            Rule::PendingMoves => line.find("closing inventory with ").is_some_and(|at| {
                let rest = &line[at + "closing inventory with ".len()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<u32>().is_ok_and(|n| n > 0)
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct LogFinding {
    pub rule: String,
    pub line: String,
}

/// Reads the part of a log written after `offset`, and returns the findings of `rules` and the lines holding each of `required`.
pub fn scan_log(path: &Path, offset: u64, rules: &[(&str, Rule)], required: &[String]) -> io::Result<(Vec<LogFinding>, BTreeMap<String, bool>)> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(bytes.get(offset as usize..).unwrap_or_default());
    let mut findings = Vec::new();
    let mut seen: BTreeMap<String, bool> = required.iter().map(|text| (text.clone(), false)).collect();
    for line in text.lines() {
        for (name, rule) in rules {
            if rule.matches(line) {
                findings.push(LogFinding { rule: (*name).to_string(), line: line.chars().take(300).collect() });
            }
        }
        for (text, found) in &mut seen {
            *found |= line.contains(text.as_str());
        }
    }
    Ok((findings, seen))
}

/// Where the client is told to find the pipes. A native harness cannot create a Windows named pipe for a client running under
/// Wine; this makes FIFOs and names them with a Windows path (`Z:` is the root of the host under Wine). UNVERIFIED that the
/// client's `CreateFileA` opens a FIFO that way.
pub fn windows_path(path: &Path) -> String {
    format!("Z:{}", path.display().to_string().replace('/', "\\"))
}

pub fn make_fifo(path: &Path) -> io::Result<()> {
    let _ = std::fs::remove_file(path);
    let status = std::process::Command::new("mkfifo").arg(path).status()?;
    status.success().then_some(()).ok_or_else(|| io::Error::other(format!("mkfifo {}", path.display())))
}

/// Opens a FIFO for reading and writing, which does not block for the other end.
pub async fn open_fifo(path: &Path) -> io::Result<tokio::fs::File> {
    tokio::fs::OpenOptions::new().read(true).write(true).open(path).await
}

#[derive(Debug, Serialize)]
pub struct HeadlessReport {
    pub unverified: &'static str,
    pub connected: bool,
    pub steps: Vec<Step>,
    pub log_findings: Vec<LogFinding>,
    pub log_required: BTreeMap<String, bool>,
    pub passed: bool,
}

#[derive(Debug, Serialize)]
pub struct Step {
    pub name: String,
    pub result: Option<i64>,
    pub ok: bool,
}

pub const UNVERIFIED: &str = "everything about the live client in this report is unverified: the harness follows static analysis only";

/// The headless client's default flags: the ones the notes name. The pipe flags and the endpoint flags are added by the caller.
pub const DEFAULT_CLIENT_ARGS: [&str; 4] = ["--client-headless-enabled", "1", "--client-login-show-dialog", "0"];

pub fn log_path_of(arg: &str) -> PathBuf {
    PathBuf::from(arg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, split};

    #[test]
    fn a_command_is_compact_json_and_a_nul() {
        let bytes = encode_command(7, "console", "command", json!({"value": "r_displayinfo 3"}));
        assert_eq!(bytes.last(), Some(&0));
        let value: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(value, json!({"command_id": 7, "adapter_name": "console", "command_name": "command", "command_args": {"value": "r_displayinfo 3"}}));
        assert!(!bytes.contains(&b' ') || String::from_utf8_lossy(&bytes).contains("r_displayinfo 3"), "compact apart from the text");
    }

    #[test]
    fn the_clients_messages_are_told_apart() {
        assert_eq!(parse_incoming(br#"{"type":"TestSystem","name":"connected"}"#), Ok(Incoming::Connected));
        assert_eq!(parse_incoming(br#"{"type":"command_response","command_id":3,"result":4,"details":null}"#), Ok(Incoming::Response { command_id: 3, result: 4, details: Value::Null }));
        assert_eq!(
            parse_incoming(br#"{"type":"CigProfile","name":"frame","content":{"ms":3}}"#),
            Ok(Incoming::Event { category: "CigProfile".into(), name: "frame".into(), content: json!({"ms": 3}) })
        );
        assert!(parse_incoming(b"not json").is_err());
    }

    #[test]
    fn messages_split_across_reads_are_put_together() {
        let mut framer = NulFramer::default();
        framer.push(b"{\"a\":1}\0{\"b\"");
        assert_eq!(framer.next_message(), Some(b"{\"a\":1}".to_vec()));
        assert_eq!(framer.next_message(), None);
        framer.push(b":2}\0");
        assert_eq!(framer.next_message(), Some(b"{\"b\":2}".to_vec()));
    }

    /// A fake client on the other end of two in-memory pipes: connected, then answers `map` commands by state.
    #[tokio::test]
    async fn the_harness_waits_for_connected_and_retries_a_command_until_the_state_takes_it() {
        let (harness_in, client_out) = duplex(4096); // client writes, harness reads
        let (client_in, harness_out) = duplex(4096); // harness writes, client reads
        let (from_client, _) = split(harness_in);
        let (_, to_client) = split(harness_out);
        let mut harness = Harness::new(from_client, to_client);

        let client = tokio::spawn(async move {
            let (_, mut out) = split(client_out);
            let (mut input, _) = split(client_in);
            out.write_all(b"{\"type\":\"TestSystem\",\"name\":\"connected\"}\0").await.unwrap();
            let mut framer = NulFramer::default();
            let mut goto_attempts = 0;
            loop {
                let mut chunk = [0u8; 512];
                let n = input.read(&mut chunk).await.unwrap();
                if n == 0 {
                    return goto_attempts;
                }
                framer.push(&chunk[..n]);
                while let Some(message) = framer.next_message() {
                    let command: Value = serde_json::from_slice(&message).unwrap();
                    let id = command["command_id"].as_i64().unwrap();
                    let code = match (command["adapter_name"].as_str().unwrap(), command["command_name"].as_str().unwrap()) {
                        ("console", "command") => result::OK,
                        ("map", "goto_global") => {
                            goto_attempts += 1;
                            if goto_attempts < 3 { result::FAILED } else { result::OK }
                        }
                        ("map", _) => result::NOT_SUPPORTED,
                        _ => result::ADAPTER_NOT_FOUND,
                    };
                    let reply = json!({"type": "command_response", "command_id": id, "result": code, "details": null});
                    let mut bytes = serde_json::to_vec(&reply).unwrap();
                    bytes.push(0);
                    out.write_all(&bytes).await.unwrap();
                }
            }
        });

        assert!(harness.wait_connected(Duration::from_secs(2)).await.unwrap());
        assert_eq!(harness.console("r_displayinfo 3", Duration::from_secs(2)).await.unwrap(), Some((result::OK, Value::Null)));
        let goto = json!({"position": {"x": 1.0, "y": 2.0, "z": 3.0}});
        let answer = harness.command_when_ready("map", "goto_global", goto, Duration::from_secs(5), Duration::from_millis(10)).await.unwrap();
        assert_eq!(answer, Some((result::OK, Value::Null)));
        assert_eq!(harness.command("nothing", "x", json!({}), Duration::from_secs(2)).await.unwrap().map(|a| a.0), Some(result::ADAPTER_NOT_FOUND));
        drop(harness);
        assert_eq!(client.await.unwrap(), 3);
    }

    #[test]
    fn the_log_rules_find_what_the_notes_name() {
        let rules = default_forbidden();
        let hit = |line: &str| rules.iter().any(|(_, rule)| rule.matches(line));
        assert!(hit("Close Inventory Grid ... closing inventory with 2 pending moves"));
        assert!(!hit("Close Inventory Grid ... closing inventory with 0 pending moves"));
        assert!(hit("[NO ATC] ShipSelectorKiosk_Hangar_HighTech_1_a-032 [226553665713] - NO LINK TO ATC"));
        assert!(hit("Snapshot Update For Missing Entity 12"));
        assert!(!hit("Player Inventory Request Complete ... Result[Succeed]"));
    }

    #[test]
    fn only_the_part_of_the_log_after_the_offset_counts() {
        let path = std::env::temp_dir().join(format!("sc-bot-log-{}.log", std::process::id()));
        std::fs::write(&path, "old [NO ATC] line\n").unwrap();
        let offset = std::fs::metadata(&path).unwrap().len();
        std::fs::write(&path, "old [NO ATC] line\nnew Spawned! line\nand Snapshot Update For Missing Entity\n").unwrap();
        let (findings, seen) = scan_log(&path, offset, &default_forbidden(), &["Spawned!".to_string(), "never".to_string()]).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(findings.len(), 1);
        assert!(seen["Spawned!"]);
        assert!(!seen["never"]);
    }

    #[test]
    fn a_fifo_is_named_with_a_wine_path() {
        assert_eq!(windows_path(Path::new("/run/user/1/sc/in")), "Z:\\run\\user\\1\\sc\\in");
    }
}
