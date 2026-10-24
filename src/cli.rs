//! Command-line interface. Every read command takes `--json`, so the CLI
//! doubles as the API for scripts and for agents driving other agents.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::client::conn::Conn;
use crate::config::Config;
use crate::protocol::{AgentState, Command, NewPane, PaneInfo, Reply};
use crate::{integrate, paths, server};

#[derive(Parser, Debug)]
#[command(
    name = "corral",
    version,
    about = "A terminal runtime for coding agents",
    long_about = "A terminal runtime for coding agents.\n\n\
        Run `corral` to attach (a background server starts automatically).\n\
        Agents keep running when you detach, close the terminal or lose SSH."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Attach the terminal UI (the default when no command is given)
    Attach {
        /// Focus the first pane of this workspace
        workspace: Option<String>,
    },
    /// List panes
    #[command(alias = "list")]
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Start an agent in a new pane
    New(NewArgs),
    /// Type text into a pane
    Send {
        pane: String,
        text: String,
        /// Press Enter after the text
        #[arg(long, short)]
        enter: bool,
    },
    /// Print a pane's screen as text
    Read {
        pane: String,
        /// Only the last N lines
        #[arg(long, short = 'n')]
        lines: Option<usize>,
        /// Include scrollback history
        #[arg(long)]
        scrollback: bool,
        #[arg(long)]
        json: bool,
    },
    /// Wait until a pane reaches a state
    Wait {
        pane: String,
        #[arg(long, value_enum)]
        state: AgentState,
        /// Give up after this many seconds (exit code 2)
        #[arg(long)]
        timeout: Option<f64>,
        #[arg(long)]
        json: bool,
    },
    /// Close a pane, ending its process
    Kill { pane: String },
    /// Rename a pane
    Rename { pane: String, name: String },
    /// Report agent state from a hook (uses $CORRAL_PANE)
    Report {
        #[arg(long, value_enum)]
        state: AgentState,
        #[arg(long)]
        pane: Option<String>,
        /// Called from an agent hook: never fail, read hook JSON from stdin
        #[arg(long)]
        hook: bool,
        /// Extra arguments some agents append (e.g. Codex's notify JSON)
        #[arg(hide = true, trailing_var_arg = true, allow_hyphen_values = true)]
        extra: Vec<String>,
    },
    /// Install hooks so an agent reports its own state
    Integrate {
        #[arg(value_enum)]
        agent: IntegrateAgent,
        /// Remove corral's hooks instead
        #[arg(long)]
        uninstall: bool,
        /// Don't ask for confirmation
        #[arg(long, short)]
        yes: bool,
    },
    /// Run the server in the foreground
    Server {
        /// Detach from the terminal and log to ~/.corral/log/server.log
        #[arg(long)]
        daemon: bool,
    },
    /// Stop the server and every agent it runs (`corral restore` brings
    /// them back)
    KillServer,
    /// Bring back the panes from before corral last stopped: agents resume
    /// their conversations, other panes come back as a shell
    Restore {
        /// Let them go instead
        #[arg(long)]
        discard: bool,
    },
    /// Use corral from your phone: serves a web app to add to its home screen
    #[cfg(feature = "web")]
    Web(WebArgs),
    /// Debugging helpers
    #[command(subcommand, hide = true)]
    Debug(DebugCmd),
}

#[cfg(feature = "web")]
#[derive(Args, Debug)]
pub struct WebArgs {
    #[command(subcommand)]
    pub cmd: Option<WebCmd>,
    /// Port to listen on
    #[arg(long, default_value_t = crate::web::DEFAULT_PORT)]
    pub port: u16,
    /// Address to listen on; anything but loopback also needs --expose
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: std::net::IpAddr,
    /// Listen on a non-loopback --bind address, over plain HTTP
    #[arg(long)]
    pub expose: bool,
    /// The address your phone opens (default: this machine's Tailscale name)
    #[arg(long)]
    pub url: Option<String>,
    /// Keep running in the background (logs to ~/.corral/log/web.log)
    #[arg(long)]
    pub daemon: bool,
    #[arg(long, hide = true)]
    pub detached: bool,
}

#[cfg(feature = "web")]
#[derive(Subcommand, Debug)]
pub enum WebCmd {
    /// Show a new pairing code, for another phone
    Pair,
    /// List the paired phones
    Devices,
    /// Unpair a phone
    Revoke { name: String },
    /// Stop `corral web --daemon`
    Stop,
}

#[derive(Args, Debug)]
pub struct NewArgs {
    #[arg(long)]
    pub name: Option<String>,
    /// Working directory (default: current directory)
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Workspace to group the pane under (default: directory name)
    #[arg(long)]
    pub workspace: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// Command to run (default: your shell)
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum DebugCmd {
    /// Write a pane's screen as replayable bytes (for rule fixtures)
    Dump {
        pane: String,
        /// Output file (default: stdout)
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum IntegrateAgent {
    Claude,
    Codex,
}

pub fn run(cli: Cli) -> Result<ExitCode> {
    let cmd = cli.command.unwrap_or(Cmd::Attach { workspace: None });
    match cmd {
        Cmd::Server { daemon } => return run_server(daemon),
        Cmd::Integrate {
            agent,
            uninstall,
            yes,
        } => {
            return integrate::run_cli(
                match agent {
                    IntegrateAgent::Claude => integrate::Agent::Claude,
                    IntegrateAgent::Codex => integrate::Agent::Codex,
                },
                uninstall,
                yes,
            )
            .map(|_| ExitCode::SUCCESS);
        }
        #[cfg(feature = "web")]
        Cmd::Web(args) => return run_web(args).map(|_| ExitCode::SUCCESS),
        _ => {}
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(run_client(cmd))
}

fn run_server(daemon: bool) -> Result<ExitCode> {
    let config = Config::load()?;
    let socket = paths::socket_path();
    if daemon {
        let dir = paths::log_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let log = dir.join("server.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .with_context(|| format!("opening {}", log.display()))?;
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
        if let Some(home) = dirs::home_dir() {
            let _ = std::env::set_current_dir(home);
        }
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_writer(std::io::stderr)
            .init();
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(server::run(socket, config))?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(feature = "web")]
fn run_web(a: WebArgs) -> Result<()> {
    use crate::web;
    match a.cmd {
        Some(WebCmd::Pair) => {
            let url =
                web::last_url().unwrap_or_else(|| web::public_url(None, (a.bind, a.port).into()));
            if web::running_pid().is_none() {
                println!("corral web isn't running; start it with `corral web` first.");
            }
            return web::print_pairing(&url);
        }
        Some(WebCmd::Devices) => return web::list_devices(),
        Some(WebCmd::Revoke { name }) => return web::revoke(&name),
        Some(WebCmd::Stop) => return web::stop(),
        None => {}
    }
    web::check_bind(a.bind, a.expose)?;
    let addr = std::net::SocketAddr::from((a.bind, a.port));
    let url = web::public_url(a.url.clone(), addr);
    if a.daemon {
        if let Some(pid) = web::running_pid() {
            bail!("corral web is already running (pid {pid}); `corral web stop` stops it");
        }
        let mut args = vec![
            "--port".to_string(),
            a.port.to_string(),
            "--bind".into(),
            a.bind.to_string(),
            "--url".into(),
            url.clone(),
        ];
        if a.expose {
            args.push("--expose".into());
        }
        web::spawn_daemon(&args)?;
        web::print_banner(addr, &url)?;
        println!("\nRunning in the background; `corral web stop` stops it.");
        return Ok(());
    }
    if a.detached {
        let dir = paths::log_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_env("CORRAL_LOG")
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_writer(std::io::stderr)
            .init();
    }
    web::serve(addr, url, a.detached)
}

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_env("CORRAL_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

async fn connect(autostart: bool) -> Result<Conn> {
    let socket = paths::socket_path();
    if autostart {
        Conn::connect_or_start(&socket).await
    } else {
        Conn::connect(&socket).await
    }
}

/// Accept `p3`, `3`, or a unique pane name.
async fn resolve_pane(conn: &mut Conn, arg: &str) -> Result<String> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = arg.strip_prefix('p')
        && digits(rest)
    {
        return Ok(arg.to_string());
    }
    if digits(arg) {
        return Ok(format!("p{arg}"));
    }
    let panes = list(conn).await?;
    let matches: Vec<&PaneInfo> = panes.iter().filter(|p| p.name == arg).collect();
    match matches.as_slice() {
        [p] => Ok(p.id.clone()),
        [] => bail!("no pane named {arg:?}"),
        _ => bail!(
            "{} panes are named {arg:?}; use an id ({})",
            matches.len(),
            matches
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

async fn list(conn: &mut Conn) -> Result<Vec<PaneInfo>> {
    match conn.request(Command::List).await? {
        Reply::List { panes } => Ok(panes),
        other => bail!("unexpected reply {other:?}"),
    }
}

fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

async fn run_client(cmd: Cmd) -> Result<ExitCode> {
    match cmd {
        Cmd::Attach { workspace } => {
            crate::client::run(workspace).await?;
        }
        Cmd::Ls { json } => {
            let mut c = connect(false).await?;
            let panes = list(&mut c).await?;
            if json {
                print_json(&panes)?;
            } else {
                print!("{}", format_table(&panes));
            }
        }
        Cmd::New(a) => {
            let mut c = connect(true).await?;
            let cwd = match a.cwd {
                Some(d) => d,
                None => std::env::current_dir().context("reading the current directory")?,
            };
            let req = NewPane {
                name: a.name,
                cwd: Some(cwd),
                workspace: a.workspace,
                command: a.command,
                shell: false,
                then_shell: false,
                focus: false,
            };
            match c.request(Command::New(req)).await? {
                Reply::Created { pane } => {
                    if a.json {
                        print_json(&pane)?;
                    } else {
                        // stdout is just the id, so `p=$(corral new …)` works.
                        println!("{}", pane.id);
                    }
                }
                other => bail!("unexpected reply {other:?}"),
            }
        }
        Cmd::Send { pane, text, enter } => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            if !text.is_empty() {
                c.request(Command::Send {
                    pane: pane.clone(),
                    bytes: text.into_bytes(),
                })
                .await?;
            }
            if enter {
                // A separate write, so TUIs see a keypress rather than a
                // pasted newline.
                tokio::time::sleep(Duration::from_millis(120)).await;
                c.request(Command::Send {
                    pane,
                    bytes: b"\r".to_vec(),
                })
                .await?;
            }
        }
        Cmd::Read {
            pane,
            lines,
            scrollback,
            json,
        } => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            let text = match c
                .request(Command::Read {
                    pane: pane.clone(),
                    lines,
                    scrollback,
                })
                .await?
            {
                Reply::Text(t) => t,
                other => bail!("unexpected reply {other:?}"),
            };
            if json {
                print_json(&serde_json::json!({ "pane": pane, "text": text }))?;
            } else {
                print!("{text}");
            }
        }
        Cmd::Wait {
            pane,
            state,
            timeout,
            json,
        } => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            let fut = c.request(Command::Wait {
                pane: pane.clone(),
                state,
            });
            let reply = match timeout {
                Some(secs) => {
                    match tokio::time::timeout(Duration::from_secs_f64(secs), fut).await {
                        Ok(r) => r?,
                        Err(_) => {
                            if json {
                                print_json(
                                    &serde_json::json!({ "pane": pane, "timed_out": true }),
                                )?;
                            } else {
                                eprintln!("corral: timed out waiting for {pane} to be {state}");
                            }
                            return Ok(ExitCode::from(2));
                        }
                    }
                }
                None => fut.await?,
            };
            if let Reply::State(s) = reply {
                if json {
                    print_json(&serde_json::json!({ "pane": pane, "state": s }))?;
                } else {
                    println!("{s}");
                }
            }
        }
        Cmd::Kill { pane } => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            if let Reply::Text(t) = c.request(Command::Kill { pane }).await? {
                println!("{t}");
            }
        }
        Cmd::Rename { pane, name } => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            c.request(Command::Rename { pane, name }).await?;
        }
        Cmd::Report {
            state,
            pane,
            hook,
            extra: _,
        } => {
            let result = report(state, pane, hook).await;
            if hook {
                // Hooks must never break the agent that runs them.
                if let Err(e) = result {
                    tracing::debug!("report failed: {e:#}");
                }
            } else {
                result?;
            }
        }
        Cmd::Restore { discard } => {
            let mut c = connect(true).await?;
            if let Reply::Text(t) = c.request(Command::Restore { discard }).await? {
                println!("{t}");
            }
        }
        Cmd::KillServer => {
            let socket = paths::socket_path();
            match connect(false).await {
                Ok(mut c) => {
                    c.request(Command::KillServer).await?;
                }
                // A server from before an update won't take orders from this
                // version: stop it with a signal instead.
                Err(e)
                    if e.chain()
                        .any(|c| c.is::<crate::protocol::VersionMismatch>()) =>
                {
                    let pid = crate::client::conn::stop_by_signal(&socket)?;
                    println!("stopped the server from another corral version (pid {pid})");
                }
                Err(e) => return Err(e),
            }
            // Wait for the socket to disappear so a following start is clean.
            for _ in 0..250 {
                if !socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        Cmd::Debug(DebugCmd::Dump { pane, output }) => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            if let Reply::Bytes(b) = c.request(Command::DebugDump { pane }).await? {
                match output {
                    Some(path) => std::fs::write(&path, &b)
                        .with_context(|| format!("writing {}", path.display()))?,
                    None => std::io::stdout().write_all(&b)?,
                }
            }
        }
        Cmd::Server { .. } | Cmd::Integrate { .. } => unreachable!(),
        #[cfg(feature = "web")]
        Cmd::Web(_) => unreachable!(),
    }
    Ok(ExitCode::SUCCESS)
}

async fn report(state: AgentState, pane: Option<String>, hook: bool) -> Result<()> {
    let pane = match pane.or_else(|| std::env::var(paths::PANE_ENV).ok()) {
        Some(p) if !p.is_empty() => p,
        // Not inside a corral pane: nothing to report.
        _ if hook => return Ok(()),
        _ => bail!("no pane given and ${} is not set", paths::PANE_ENV),
    };
    let input = if hook { hook_input() } else { None };
    // Claude Code's Notification hook also fires for "waiting for your
    // input" after an idle minute; that isn't a blocked agent.
    if hook && state == AgentState::Blocked && !input.as_deref().is_none_or(hook_json_is_blocking) {
        return Ok(());
    }
    let session = input.as_deref().and_then(hook_session);
    let mut c = connect(false).await?;
    c.request(Command::Report {
        pane,
        state,
        session,
    })
    .await?;
    Ok(())
}

/// The JSON a hook gets on stdin (Claude Code sends some); `None` from a
/// terminal or if nothing arrives within a second.
fn hook_input() -> Option<String> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::stdin().take(1 << 20).read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    rx.recv_timeout(Duration::from_secs(1)).ok()
}

/// The agent's session id in hook JSON (`session_id`, as Claude sends).
pub fn hook_session(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let id = v.get("session_id")?.as_str()?;
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| id.to_string())
}

pub fn hook_json_is_blocking(json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return true;
    };
    // Claude Code tags each notification; only these two wait on the user
    // (the others are e.g. `idle_prompt` and `auth_success`).
    match v.get("notification_type").and_then(|t| t.as_str()) {
        Some("permission_prompt" | "elicitation_dialog") => return true,
        Some(t) if !t.is_empty() => return false,
        _ => {}
    }
    let msg = v
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_lowercase();
    !msg.contains("waiting for your input")
}

fn home_relative(p: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = p.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    p.display().to_string()
}

fn table(rows: Vec<Vec<String>>) -> String {
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for r in rows {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

pub fn format_table(panes: &[PaneInfo]) -> String {
    if panes.is_empty() {
        return "no panes\n".into();
    }
    let mut rows = vec![
        ["ID", "NAME", "STATE", "AGENT", "WORKSPACE", "CWD"]
            .map(String::from)
            .to_vec(),
    ];
    for p in panes {
        let state = match (p.state, p.exit_code) {
            (AgentState::Exited, Some(c)) => format!("exited({c})"),
            (s, _) => s.to_string(),
        };
        rows.push(vec![
            p.id.clone(),
            p.name.clone(),
            state,
            p.agent.clone().unwrap_or_else(|| "-".into()),
            p.workspace.clone(),
            home_relative(&p.cwd),
        ]);
    }
    table(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_new_with_command() {
        let cli =
            Cli::try_parse_from(["corral", "new", "--", "claude", "--model", "opus"]).unwrap();
        match cli.command {
            Some(Cmd::New(a)) => {
                assert_eq!(a.command, vec!["claude", "--model", "opus"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn report_accepts_codex_notify_payload() {
        let cli = Cli::try_parse_from([
            "corral",
            "report",
            "--state",
            "done",
            "--hook",
            r#"{"type":"agent-turn-complete"}"#,
        ])
        .unwrap();
        assert!(matches!(cli.command, Some(Cmd::Report { hook: true, .. })));
    }

    #[test]
    fn hook_session_ids() {
        assert_eq!(
            hook_session(r#"{"session_id":"8f2c-11aa","hook_event_name":"Stop"}"#).as_deref(),
            Some("8f2c-11aa")
        );
        assert_eq!(hook_session(r#"{"session_id":"x; rm -rf /"}"#), None);
        assert_eq!(hook_session("not json"), None);
    }

    #[test]
    fn idle_notifications_are_not_blocking() {
        assert!(hook_json_is_blocking(
            r#"{"message":"Claude needs your permission to use Bash"}"#
        ));
        assert!(!hook_json_is_blocking(
            r#"{"message":"Claude is waiting for your input"}"#
        ));
        assert!(!hook_json_is_blocking(
            r#"{"notification_type":"idle_prompt","message":"x"}"#
        ));
        assert!(!hook_json_is_blocking(
            r#"{"notification_type":"auth_success","message":"Signed in"}"#
        ));
        assert!(hook_json_is_blocking(
            r#"{"notification_type":"permission_prompt","message":"x"}"#
        ));
        assert!(hook_json_is_blocking(
            r#"{"notification_type":"elicitation_dialog","message":"x"}"#
        ));
        assert!(hook_json_is_blocking("not json"));
    }

    #[test]
    fn table_format() {
        let p = PaneInfo {
            id: "p1".into(),
            name: "claude".into(),
            workspace: "corral".into(),
            command: vec!["claude".into()],
            cwd: "/tmp".into(),
            agent: Some("claude".into()),
            state: AgentState::Exited,
            pid: None,
            exit_code: Some(1),
            title: None,
            git: None,
            cols: 80,
            rows: 24,
            state_secs: 0,
        };
        let t = format_table(&[p]);
        assert!(t.starts_with("ID  NAME"), "{t}");
        assert!(t.contains("exited(1)"), "{t}");
    }
}
