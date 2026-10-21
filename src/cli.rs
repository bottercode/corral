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
use crate::protocol::{
    AgentState, Command, DiffReport, NewPane, PaneInfo, Reply, WorktreeAction, WorktreeInfo,
};
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
    Kill {
        pane: String,
        /// Also remove the pane's worktree (refuses if it has uncommitted changes)
        #[arg(long)]
        rm_worktree: bool,
        /// Remove the worktree and delete its branch (refuses if unmerged)
        #[arg(long)]
        delete_branch: bool,
        /// Remove even with uncommitted or unmerged work
        #[arg(long)]
        force: bool,
    },
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
    /// Show what an agent changed in its worktree
    Diff {
        pane: String,
        /// Only list files and line counts
        #[arg(long)]
        stat: bool,
        #[arg(long)]
        json: bool,
    },
    /// Commit everything in an agent's worktree
    Commit {
        pane: String,
        #[arg(long, short)]
        message: String,
    },
    /// Merge an agent's branch into the branch its worktree came from
    Merge { pane: String },
    /// Throw away uncommitted changes in an agent's worktree
    Discard {
        pane: String,
        /// Required: this can't be undone
        #[arg(long)]
        yes: bool,
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
    /// Manage agent worktrees
    #[command(subcommand)]
    Worktree(WorktreeCmd),
    /// Run the server in the foreground
    Server {
        /// Detach from the terminal and log to ~/.corral/log/server.log
        #[arg(long)]
        daemon: bool,
    },
    /// Stop the server and every agent it runs
    KillServer,
    /// Debugging helpers
    #[command(subcommand, hide = true)]
    Debug(DebugCmd),
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
    /// Run the agent in a new git worktree
    #[arg(long, short)]
    pub worktree: bool,
    /// Branch for the worktree (default: corral/<agent>-<id>)
    #[arg(long, requires = "worktree")]
    pub branch: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// Command to run (default: your shell)
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum WorktreeCmd {
    /// List worktrees corral created
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Remove a worktree by pane id or path
    Rm {
        target: String,
        #[arg(long)]
        force: bool,
        /// Also delete the branch
        #[arg(long)]
        delete_branch: bool,
    },
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
    let _guard = if daemon {
        let dir = paths::log_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let appender = tracing_appender::rolling::never(&dir, "server.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_writer(writer)
            .with_ansi(false)
            .init();
        if let Some(home) = dirs::home_dir() {
            let _ = std::env::set_current_dir(home);
        }
        Some(guard)
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter())
            .with_writer(std::io::stderr)
            .init();
        None
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(server::run(socket, config))?;
    Ok(ExitCode::SUCCESS)
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
                worktree: a.worktree,
                branch: a.branch,
                focus: false,
            };
            match c.request(Command::New(req)).await? {
                Reply::Created { pane, warning } => {
                    if let Some(w) = warning {
                        eprintln!("corral: warning: {w}");
                    }
                    if a.json {
                        print_json(&pane)?;
                    } else {
                        // stdout is just the id, so `p=$(corral new …)` works.
                        println!("{}", pane.id);
                        if let Some(w) = &pane.worktree {
                            eprintln!("worktree {} on branch {}", home_relative(&w.path), w.branch);
                        }
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
        Cmd::Kill {
            pane,
            rm_worktree,
            delete_branch,
            force,
        } => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            let worktree = if delete_branch {
                WorktreeAction::RemoveAndDeleteBranch
            } else if rm_worktree {
                WorktreeAction::Remove
            } else {
                WorktreeAction::Keep
            };
            if let Reply::Text(t) = c
                .request(Command::Kill {
                    pane,
                    worktree,
                    force,
                })
                .await?
            {
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
        Cmd::Worktree(WorktreeCmd::Ls { json }) => {
            let list = match connect(false).await {
                Ok(mut c) => match c.request(Command::WorktreeList).await? {
                    Reply::Worktrees(w) => w,
                    other => bail!("unexpected reply {other:?}"),
                },
                // No server: still list what's on disk.
                Err(_) => crate::worktree::list(&Config::load()?.worktree_root()),
            };
            if json {
                print_json(&list)?;
            } else {
                print!("{}", format_worktrees(&list));
            }
        }
        Cmd::Worktree(WorktreeCmd::Rm {
            target,
            force,
            delete_branch,
        }) => {
            let looks_like_pane =
                target.starts_with('p') && target[1..].bytes().all(|b| b.is_ascii_digit());
            match connect(false).await {
                Ok(mut c) => {
                    if let Reply::Text(t) = c
                        .request(Command::WorktreeRemove {
                            target,
                            force,
                            delete_branch,
                        })
                        .await?
                    {
                        println!("{t}");
                    }
                }
                Err(e) if looks_like_pane => return Err(e),
                Err(_) => {
                    let path = PathBuf::from(&target);
                    crate::worktree::remove(
                        &path,
                        crate::worktree::RemoveOpts {
                            force,
                            delete_branch,
                        },
                    )?;
                    println!("removed {}", path.display());
                }
            }
        }
        Cmd::KillServer => {
            let mut c = connect(false).await?;
            c.request(Command::KillServer).await?;
            // Wait for the socket to disappear so a following start is clean.
            let socket = paths::socket_path();
            for _ in 0..50 {
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
        Cmd::Diff { pane, stat, json } => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            let Reply::Diff(report) = c.request(Command::Diff { pane }).await? else {
                bail!("unexpected reply");
            };
            if json {
                print_json(&report)?;
            } else {
                print!(
                    "{}",
                    format_diff(&report, stat, std::io::stdout().is_terminal())
                );
            }
        }
        Cmd::Commit { pane, message } => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            if let Reply::Text(t) = c.request(Command::Commit { pane, message }).await? {
                println!("{t}");
            }
        }
        Cmd::Merge { pane } => {
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            if let Reply::Text(t) = c.request(Command::Merge { pane }).await? {
                println!("{t}");
            }
        }
        Cmd::Discard { pane, yes } => {
            if !yes {
                bail!("this throws away the agent's uncommitted work; pass --yes to confirm");
            }
            let mut c = connect(false).await?;
            let pane = resolve_pane(&mut c, &pane).await?;
            if let Reply::Text(t) = c.request(Command::Discard { pane }).await? {
                println!("{t}");
            }
        }
        Cmd::Server { .. } | Cmd::Integrate { .. } => unreachable!(),
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
    if hook && state == AgentState::Blocked && !hook_input_is_blocking() {
        return Ok(());
    }
    let mut c = connect(false).await?;
    c.request(Command::Report { pane, state }).await?;
    Ok(())
}

/// Claude Code's Notification hook also fires for "waiting for your input"
/// after an idle minute; that isn't a blocked agent.
fn hook_input_is_blocking() -> bool {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return true;
    }
    let mut buf = String::new();
    let _ = stdin.take(1 << 20).read_to_string(&mut buf);
    hook_json_is_blocking(&buf)
}

pub fn hook_json_is_blocking(json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return true;
    };
    let kind = v
        .get("notification_type")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let msg = v
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_lowercase();
    !(kind == "idle_prompt" || msg.contains("waiting for your input"))
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
        [
            "ID",
            "NAME",
            "STATE",
            "AGENT",
            "WORKSPACE",
            "WORKTREE",
            "CWD",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for p in panes {
        let state = match (p.state, p.exit_code) {
            (AgentState::Exited, Some(c)) => format!("exited({c})"),
            (s, _) => s.to_string(),
        };
        let wt = p
            .worktree
            .as_ref()
            .map(|w| format!("{} +{}", w.branch, w.changed_files))
            .unwrap_or_else(|| "-".into());
        rows.push(vec![
            p.id.clone(),
            p.name.clone(),
            state,
            p.agent.clone().unwrap_or_else(|| "-".into()),
            p.workspace.clone(),
            wt,
            home_relative(&p.cwd),
        ]);
    }
    table(rows)
}

/// Human-readable diff report, coloured for terminals.
pub fn format_diff(r: &DiffReport, stat: bool, color: bool) -> String {
    let paint = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    let added: u32 = r.files.iter().map(|f| f.added).sum();
    let removed: u32 = r.files.iter().map(|f| f.removed).sum();
    let mut out = format!(
        "{} → {} · {} files +{added} −{removed} · {} commits · {} uncommitted\n",
        paint("35", &r.branch),
        r.base,
        r.files.len(),
        r.commits,
        r.uncommitted
    );
    for f in &r.files {
        let counts = if f.binary {
            "binary".to_string()
        } else {
            format!("+{} −{}", f.added, f.removed)
        };
        out.push_str(&format!(
            "{} {}  {counts}\n",
            paint("33", &f.status),
            paint("1", &f.path)
        ));
        if stat || f.binary {
            continue;
        }
        for line in f.patch.lines() {
            let code = if line.starts_with("@@") {
                "36"
            } else if line.starts_with('+') {
                "32"
            } else if line.starts_with('-') {
                "31"
            } else {
                ""
            };
            if code.is_empty() {
                out.push_str(line);
            } else {
                out.push_str(&paint(code, line));
            }
            out.push('\n');
        }
    }
    out
}

pub fn format_worktrees(list: &[WorktreeInfo]) -> String {
    if list.is_empty() {
        return "no worktrees\n".into();
    }
    let mut rows = vec![
        ["BRANCH", "CHANGED", "PANE", "PATH"]
            .map(String::from)
            .to_vec(),
    ];
    for w in list {
        rows.push(vec![
            w.branch.clone(),
            w.changed_files.to_string(),
            w.pane.clone().unwrap_or_else(|| "-".into()),
            home_relative(&w.path),
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
        let cli = Cli::try_parse_from([
            "corral",
            "new",
            "--worktree",
            "--branch",
            "x",
            "--",
            "claude",
            "--model",
            "opus",
        ])
        .unwrap();
        match cli.command {
            Some(Cmd::New(a)) => {
                assert!(a.worktree);
                assert_eq!(a.branch.as_deref(), Some("x"));
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
            worktree: None,
            cols: 80,
            rows: 24,
            state_secs: 0,
        };
        let t = format_table(&[p]);
        assert!(t.starts_with("ID  NAME"), "{t}");
        assert!(t.contains("exited(1)"), "{t}");
    }
}
