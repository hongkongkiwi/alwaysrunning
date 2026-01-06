use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[command(
    name = "runner",
    version,
    about = "Run a binary and keep it alive.",
    arg_required_else_help = true,
    subcommand_required = true,
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Register and run an app continuously.
    Run {
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(value_name = "CMD")]
        cmd: String,
        /// Load environment variables from a file (simple KEY=VALUE format).
        #[arg(long)]
        env_file: Option<String>,
        /// Clear inherited environment before applying env_file.
        #[arg(long)]
        clean_env: bool,
        /// Keep the runner in the foreground with a live status screen.
        #[arg(long)]
        foreground: bool,
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            num_args = 0..,
            value_name = "ARGS"
        )]
        args: Vec<String>,
        #[arg(long, default_value_t = 1)]
        instances: usize,
        /// Skip autostart install.
        #[arg(long)]
        no_autostart: bool,
    },
    /// Show status of running apps.
    Status {
        name: Option<String>,
        /// Show a live status screen (like pm2 monit).
        #[arg(long)]
        watch: bool,
        /// Output status as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start a registered app.
    Start {
        name: String,
        /// Override desired instances.
        #[arg(long)]
        instances: Option<usize>,
    },
    /// Stop an app (or all apps).
    Stop {
        name: Option<String>,
        #[arg(long)]
        all: bool,
        /// Signal to send (e.g. SIGTERM, SIGKILL, TERM, 15).
        #[arg(long, default_value = "TERM")]
        signal: String,
    },
    /// Restart an app (or all apps).
    Restart {
        name: Option<String>,
        #[arg(long)]
        all: bool,
        /// Signal to send before restarting.
        #[arg(long, default_value = "TERM")]
        signal: String,
    },
    /// Delete an app (removes from config).
    Delete {
        name: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Show logs for an app.
    Logs {
        name: String,
        #[arg(long)]
        instance: Option<usize>,
        #[arg(long)]
        follow: bool,
        /// Number of lines to show (ignored with --follow).
        #[arg(long, default_value_t = 200)]
        lines: usize,
        /// Show logs since a timestamp or duration (e.g. 1700000000, 10m, 2h).
        #[arg(long)]
        since: Option<String>,
        /// Output logs as JSON lines.
        #[arg(long)]
        json: bool,
    },
    /// Export app config to a file.
    Export {
        file: String,
        name: Option<String>,
    },
    /// Import app config from a file.
    Import {
        file: String,
        /// Replace existing config instead of merging.
        #[arg(long)]
        replace: bool,
        /// Start imported apps.
        #[arg(long)]
        start: bool,
    },
    /// Alias for status --watch.
    Attach {
        name: Option<String>,
    },
    /// Alias for status.
    List {
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        json: bool,
        name: Option<String>,
    },
    /// Run the supervisor (internal).
    Daemon {
        /// Keep in the foreground (useful for launchd/systemd).
        #[arg(long)]
        foreground: bool,
        /// Show a live status screen (for foreground runs).
        #[arg(long)]
        watch: bool,
    },
    /// Install autostart for the runner daemon.
    Install,
    /// Uninstall autostart for the runner daemon.
    Uninstall,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct State {
    apps: Vec<AppConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct AppConfig {
    name: String,
    cmd: String,
    args: Vec<String>,
    instances: usize,
    created_at: u64,
    #[serde(default)]
    env_file: Option<String>,
    #[serde(default)]
    clean_env: bool,
    #[serde(default)]
    paused: bool,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Runtime {
    apps: BTreeMap<String, AppRuntime>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct AppRuntime {
    #[serde(default)]
    instances: BTreeMap<usize, InstanceRuntime>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct InstanceRuntime {
    #[serde(default)]
    pid: i32,
    #[serde(default)]
    started_at: u64,
    #[serde(default)]
    restarts: u64,
    #[serde(default)]
    last_exit_at: Option<u64>,
    #[serde(default)]
    last_exit_code: Option<i32>,
    #[serde(default)]
    last_exit_signal: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct RuntimeV0 {
    apps: BTreeMap<String, AppRuntimeV0>,
}

#[derive(Debug, Deserialize)]
struct AppRuntimeV0 {
    instances: BTreeMap<usize, i32>,
}

impl From<RuntimeV0> for Runtime {
    fn from(old: RuntimeV0) -> Self {
        let mut apps = BTreeMap::new();
        for (name, app) in old.apps {
            let mut instances = BTreeMap::new();
            for (idx, pid) in app.instances {
                instances.insert(
                    idx,
                    InstanceRuntime {
                        pid,
                        ..InstanceRuntime::default()
                    },
                );
            }
            apps.insert(name, AppRuntime { instances });
        }
        Runtime { apps }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run {
            name,
            cmd,
            env_file,
            clean_env,
            foreground,
            args,
            instances,
            no_autostart,
        } => cmd_run(
            &name,
            &cmd,
            &args,
            instances,
            env_file,
            clean_env,
            foreground,
            no_autostart,
        ),
        Commands::Status { name, watch, json } => cmd_status(name.as_deref(), watch, json),
        Commands::Start { name, instances } => cmd_start(&name, instances),
        Commands::Stop { name, all, signal } => cmd_stop(name.as_deref(), all, &signal),
        Commands::Restart { name, all, signal } => cmd_restart(name.as_deref(), all, &signal),
        Commands::Delete { name, all } => cmd_delete(name.as_deref(), all),
        Commands::Logs {
            name,
            instance,
            follow,
            lines,
            since,
            json,
        } => cmd_logs(&name, instance, follow, lines, since.as_deref(), json),
        Commands::Export { file, name } => cmd_export(&file, name.as_deref()),
        Commands::Import { file, replace, start } => cmd_import(&file, replace, start),
        Commands::Attach { name } => cmd_status(name.as_deref(), true, false),
        Commands::List { watch, json, name } => cmd_status(name.as_deref(), watch, json),
        Commands::Daemon { foreground, watch } => cmd_daemon(foreground, watch),
        Commands::Install => cmd_install(),
        Commands::Uninstall => cmd_uninstall(),
    }
}

fn cmd_run(
    name: &str,
    cmd: &str,
    args: &[String],
    instances: usize,
    env_file: Option<String>,
    clean_env: bool,
    foreground: bool,
    no_autostart: bool,
) -> Result<()> {
    if instances == 0 {
        anyhow::bail!("--instances must be >= 1");
    }
    ensure_dirs()?;
    let mut state = read_state()?;
    state.apps.retain(|a| a.name != name);
    let env_file = match env_file {
        Some(path) => {
            let p = PathBuf::from(path);
            let abs = if p.is_absolute() {
                p
            } else {
                std::env::current_dir()?.join(p)
            };
            Some(abs.to_string_lossy().to_string())
        }
        None => None,
    };
    state.apps.push(AppConfig {
        name: name.to_string(),
        cmd: cmd.to_string(),
        args: args.to_vec(),
        instances,
        created_at: now_ts(),
        env_file,
        clean_env,
        paused: false,
    });
    write_state(&state)?;
    if !no_autostart {
        let _ = cmd_install();
    }
    if foreground {
        if let Some(pid) = read_pid_file()? {
            if is_pid_alive(pid) {
                println!("Runner already running in background. Attaching status screen...");
                return status_watch_loop(None);
            }
        }
        println!("Running in foreground. Press Ctrl+C to exit.");
        return cmd_daemon(true, true);
    } else {
        ensure_daemon_running()?;
        println!(
            "OK: '{}' -> {} ({} instance{})",
            name,
            cmd,
            instances,
            if instances == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn cmd_status(name: Option<&str>, watch: bool, json: bool) -> Result<()> {
    ensure_dirs()?;
    if watch {
        return status_watch_loop(name);
    }
    let state = read_state()?;
    let runtime = read_runtime()?;
    let mut apps = state.apps;
    if let Some(name) = name {
        apps.retain(|a| a.name == name);
    }
    if apps.is_empty() {
        if json {
            let snapshot = StatusSnapshot {
                generated_at: now_ts(),
                apps: Vec::new(),
            };
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        } else {
            println!("No apps registered.");
        }
        return Ok(());
    }
    let snapshot = build_status_snapshot(&apps, &runtime);
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }
    for app in snapshot.apps {
        let paused = if app.paused { "paused " } else { "" };
        println!(
            "{}: {}/{} running  restarts={}  {}cmd: {} {}",
            app.name,
            app.running,
            app.instances_desired,
            app.total_restarts,
            paused,
            app.cmd,
            app.args.join(" ")
        );
        for inst in app.instances {
            let uptime = inst.uptime_secs.map(|u| format!("{u}s")).unwrap_or("-".to_string());
            let last_exit = inst
                .last_exit_at
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string());
            let code = inst
                .last_exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string());
            let sig = inst
                .last_exit_signal
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "  [{idx}] pid={pid:<6} {state:<4} uptime={uptime:<6} restarts={restarts:<3} last_exit_at={last_exit} code={code} sig={sig}",
                idx = inst.index,
                pid = inst.pid.unwrap_or(0),
                state = if inst.alive { "up" } else { "down" },
                uptime = uptime,
                restarts = inst.restarts
            );
        }
    }
    Ok(())
}

fn cmd_start(name: &str, instances: Option<usize>) -> Result<()> {
    ensure_dirs()?;
    let mut state = read_state()?;
    let mut found = false;
    for app in state.apps.iter_mut() {
        if app.name == name {
            found = true;
            if let Some(i) = instances {
                if i == 0 {
                    anyhow::bail!("--instances must be >= 1");
                }
                app.instances = i;
            }
            app.paused = false;
            break;
        }
    }
    if !found {
        anyhow::bail!("No app named '{}' registered. Use `runner run` first.", name);
    }
    write_state(&state)?;
    ensure_daemon_running()?;
    println!("Started {}", name);
    Ok(())
}

fn cmd_stop(name: Option<&str>, all: bool, signal: &str) -> Result<()> {
    ensure_dirs()?;
    let sig = parse_signal(signal)?;
    let mut state = read_state()?;
    let mut runtime = read_runtime()?;

    if all {
        for app in state.apps.iter_mut() {
            app.paused = true;
        }
        for (app_name, rt) in runtime.apps.iter_mut() {
            for inst in rt.instances.values_mut() {
                if inst.pid > 0 {
                    terminate_graceful(inst.pid, sig, Duration::from_secs(3));
                }
            }
            println!("Stopped {}", app_name);
        }
        write_state(&state)?;
        return Ok(());
    }

    let Some(name) = name else {
        anyhow::bail!("Provide an app name or use --all.");
    };

    for app in state.apps.iter_mut() {
        if app.name == name {
            app.paused = true;
        }
    }
    if let Some(rt) = runtime.apps.get_mut(name) {
        for inst in rt.instances.values_mut() {
            if inst.pid > 0 {
                terminate_graceful(inst.pid, sig, Duration::from_secs(3));
            }
        }
        println!("Stopped {}", name);
    } else {
        println!("No runtime for {}", name);
    }
    write_state(&state)?;
    Ok(())
}

fn cmd_restart(name: Option<&str>, all: bool, signal: &str) -> Result<()> {
    ensure_dirs()?;
    let sig = parse_signal(signal)?;
    let mut state = read_state()?;
    let mut runtime = read_runtime()?;

    if all {
        for app in state.apps.iter_mut() {
            app.paused = false;
        }
        for rt in runtime.apps.values_mut() {
            for inst in rt.instances.values_mut() {
                if inst.pid > 0 {
                    terminate_graceful(inst.pid, sig, Duration::from_secs(3));
                }
            }
        }
    } else {
        let Some(name) = name else {
            anyhow::bail!("Provide an app name or use --all.");
        };
        for app in state.apps.iter_mut() {
            if app.name == name {
                app.paused = false;
            }
        }
        if let Some(rt) = runtime.apps.get_mut(name) {
            for inst in rt.instances.values_mut() {
                if inst.pid > 0 {
                    terminate_graceful(inst.pid, sig, Duration::from_secs(3));
                }
            }
        }
    }

    write_state(&state)?;
    ensure_daemon_running()?;
    let target = if all {
        "all".to_string()
    } else {
        name.unwrap_or("app").to_string()
    };
    println!("Restarted {}", target);
    Ok(())
}

fn cmd_delete(name: Option<&str>, all: bool) -> Result<()> {
    ensure_dirs()?;
    let mut state = read_state()?;
    let mut runtime = read_runtime()?;
    if all {
        for (app_name, rt) in runtime.apps.iter_mut() {
            for inst in rt.instances.values_mut() {
                if inst.pid > 0 {
                    terminate_pid(inst.pid);
                }
            }
            println!("Deleted {}", app_name);
        }
        state.apps.clear();
        runtime.apps.clear();
        write_state(&state)?;
        write_runtime(&runtime)?;
        return Ok(());
    }
    let Some(name) = name else {
        anyhow::bail!("Provide an app name or use --all.");
    };
    state.apps.retain(|a| a.name != name);
    if let Some(rt) = runtime.apps.remove(name) {
        for inst in rt.instances.values() {
            if inst.pid > 0 {
                terminate_pid(inst.pid);
            }
        }
        println!("Deleted {}", name);
    } else {
        println!("No runtime for {}", name);
    }
    write_state(&state)?;
    write_runtime(&runtime)?;
    Ok(())
}

fn cmd_export(file: &str, name: Option<&str>) -> Result<()> {
    ensure_dirs()?;
    let mut state = read_state()?;
    if let Some(name) = name {
        state.apps.retain(|a| a.name == name);
    }
    write_json_atomic(&PathBuf::from(file), &state)?;
    println!("Exported {}", file);
    Ok(())
}

fn cmd_import(file: &str, replace: bool, start: bool) -> Result<()> {
    ensure_dirs()?;
    let incoming: State = read_json(&PathBuf::from(file))?;
    let mut state = if replace { State::default() } else { read_state()? };
    for app in incoming.apps {
        state.apps.retain(|a| a.name != app.name);
        state.apps.push(app);
    }
    write_state(&state)?;
    if start {
        ensure_daemon_running()?;
    }
    println!("Imported {}", file);
    Ok(())
}

fn cmd_logs(
    name: &str,
    instance: Option<usize>,
    follow: bool,
    lines: usize,
    since: Option<&str>,
    json: bool,
) -> Result<()> {
    ensure_dirs()?;
    let idx = instance.unwrap_or(0);
    let path = log_path(name, idx);
    if !path.exists() {
        anyhow::bail!("No log file found at {:?}", path);
    }
    let since_time = since.map(parse_since).transpose()?;

    if follow {
        if let Some(since_time) = since_time {
            let mtime = fs::metadata(&path)?.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if mtime >= since_time {
                    if json {
                        let lines = read_tail_lines(&path, lines)?;
                        for line in lines {
                            let obj = serde_json::json!({ "line": line });
                            println!("{obj}");
                        }
                    } else {
                        print_tail(&path, lines)?;
                    }
                }
            }
        follow_file(&path, json)
    } else {
        if let Some(since_time) = since_time {
            let mtime = fs::metadata(&path)?.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if mtime < since_time {
                return Ok(());
            }
        }
        if json {
            let lines = read_tail_lines(&path, lines)?;
            for line in lines {
                let obj = serde_json::json!({ "line": line });
                println!("{obj}");
            }
            Ok(())
        } else {
            print_tail(&path, lines)
        }
    }
}

fn cmd_daemon(foreground: bool, watch: bool) -> Result<()> {
    ensure_dirs()?;
    if !foreground {
        daemonize()?;
    }
    write_pid_file()?;
    daemon_loop(watch)
}

fn cmd_install() -> Result<()> {
    ensure_dirs()?;
    #[cfg(target_os = "macos")]
    {
        install_launchd()?;
    }
    #[cfg(target_os = "linux")]
    {
        install_systemd_user()?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        println!("Autostart not supported on this OS yet.");
    }
    Ok(())
}

fn cmd_uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        uninstall_launchd()?;
    }
    #[cfg(target_os = "linux")]
    {
        uninstall_systemd_user()?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        println!("Autostart not supported on this OS yet.");
    }
    Ok(())
}

fn status_watch_loop(name: Option<&str>) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        return cmd_status(name, false, false);
    }
    enable_raw_mode()?;
    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }
    let _guard = RawModeGuard;

    let filter = name.map(|s| s.to_string());
    let mut selected_app = 0usize;
    let mut selected_inst = 0usize;
    let mut show_logs = false;
    loop {
        let state = read_state()?;
        let runtime = read_runtime()?;
        let mut apps = state.apps.clone();
        if let Some(name) = filter.as_deref() {
            apps.retain(|a| a.name == name);
        }
        let snapshot = build_status_snapshot(&apps, &runtime);
        if selected_app >= snapshot.apps.len() && !snapshot.apps.is_empty() {
            selected_app = snapshot.apps.len() - 1;
        }
        render_status_screen_with_selection(&snapshot, selected_app, selected_inst, show_logs);

        if event::poll(Duration::from_millis(1000))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('r') => {
                            if filter.is_some() {
                                let _ = cmd_restart(filter.as_deref(), false, "TERM");
                            } else {
                                let _ = cmd_restart(None, true, "TERM");
                            }
                        }
                        KeyCode::Char('s') => {
                            if filter.is_some() {
                                let _ = cmd_stop(filter.as_deref(), false, "TERM");
                            } else {
                                let _ = cmd_stop(None, true, "TERM");
                            }
                        }
                        KeyCode::Char('l') => {
                            show_logs = !show_logs;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if selected_app + 1 < snapshot.apps.len() {
                                selected_app += 1;
                                selected_inst = 0;
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if selected_app > 0 {
                                selected_app -= 1;
                                selected_inst = 0;
                            }
                        }
                        KeyCode::Right | KeyCode::Char(']') => {
                            if let Some(app) = snapshot.apps.get(selected_app) {
                                if selected_inst + 1 < app.instances.len() {
                                    selected_inst += 1;
                                }
                            }
                        }
                        KeyCode::Left | KeyCode::Char('[') => {
                            if selected_inst > 0 {
                                selected_inst -= 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn render_status_screen_with_selection(
    snapshot: &StatusSnapshot,
    selected_app: usize,
    selected_inst: usize,
    show_logs: bool,
) {
    print!("\x1b[2J\x1b[H");
    let now = now_ts();
    println!("runner status  (q/ctrl+c exit, r restart, s stop, l logs, j/k app, [ ] inst)  t={now}");

    if snapshot.apps.is_empty() {
        println!("No apps registered.");
        let _ = std::io::stdout().flush();
        return;
    }

    for (i, app) in snapshot.apps.iter().enumerate() {
        let paused = if app.paused { "paused " } else { "" };
        let sel = if i == selected_app { ">" } else { " " };
        println!(
            "{sel} {}: {}/{} running  restarts={}  {}cmd: {} {}",
            app.name,
            app.running,
            app.instances_desired,
            app.total_restarts,
            paused,
            app.cmd,
            app.args.join(" ")
        );
        for (j, inst) in app.instances.iter().enumerate() {
            let uptime = inst.uptime_secs.map(|u| format!("{u}s")).unwrap_or("-".to_string());
            let pid = inst.pid.unwrap_or(0);
            let mark = if i == selected_app && j == selected_inst { "*" } else { " " };
            println!(
                "  {mark}[{idx}] pid={pid:<6} {state:<4} uptime={uptime:<6} restarts={restarts:<3}",
                idx = inst.index,
                state = if inst.alive { "up" } else { "down" },
                uptime = uptime,
                restarts = inst.restarts
            );
        }
    }

    if show_logs {
        if let Some(app) = snapshot.apps.get(selected_app) {
            let inst = app.instances.get(selected_inst);
            if let Some(inst) = inst {
                println!("\n--- logs: {}[{}] (tail 20) ---", app.name, inst.index);
                let path = log_path(&app.name, inst.index);
                match read_tail_lines(&path, 20) {
                    Ok(lines) => {
                        for line in lines {
                            println!("{line}");
                        }
                    }
                    Err(err) => {
                        println!("(unable to read log: {err})");
                    }
                }
            }
        }
    }
    let _ = std::io::stdout().flush();
}

#[derive(Debug, Serialize)]
struct StatusSnapshot {
    generated_at: u64,
    apps: Vec<StatusApp>,
}

#[derive(Debug, Serialize)]
struct StatusApp {
    name: String,
    cmd: String,
    args: Vec<String>,
    instances_desired: usize,
    running: usize,
    total_restarts: u64,
    paused: bool,
    instances: Vec<StatusInstance>,
}

#[derive(Debug, Serialize)]
struct StatusInstance {
    index: usize,
    pid: Option<i32>,
    alive: bool,
    started_at: Option<u64>,
    uptime_secs: Option<u64>,
    restarts: u64,
    last_exit_at: Option<u64>,
    last_exit_code: Option<i32>,
    last_exit_signal: Option<i32>,
}

fn build_status_snapshot(apps: &[AppConfig], runtime: &Runtime) -> StatusSnapshot {
    let now = now_ts();
    let mut out = Vec::new();
    for app in apps {
        let rt = runtime.apps.get(&app.name);
        let mut instances = Vec::new();
        let mut running = 0usize;
        let mut total_restarts = 0u64;
        for idx in 0..app.instances {
            let inst = rt.and_then(|r| r.instances.get(&idx)).cloned().unwrap_or_default();
            let alive = inst.pid > 0 && is_pid_alive(inst.pid);
            if alive {
                running += 1;
            }
            total_restarts += inst.restarts;
            let started_at = if inst.started_at > 0 { Some(inst.started_at) } else { None };
            let uptime_secs = if alive && inst.started_at > 0 {
                now.checked_sub(inst.started_at)
            } else {
                None
            };
            instances.push(StatusInstance {
                index: idx,
                pid: if inst.pid > 0 { Some(inst.pid) } else { None },
                alive,
                started_at,
                uptime_secs,
                restarts: inst.restarts,
                last_exit_at: inst.last_exit_at,
                last_exit_code: inst.last_exit_code,
                last_exit_signal: inst.last_exit_signal,
            });
        }
        out.push(StatusApp {
            name: app.name.clone(),
            cmd: app.cmd.clone(),
            args: app.args.clone(),
            instances_desired: app.instances,
            running,
            total_restarts,
            paused: app.paused,
            instances,
        });
    }
    StatusSnapshot {
        generated_at: now,
        apps: out,
    }
}

fn daemon_loop(watch: bool) -> Result<()> {
    loop {
        let state = read_state()?;
        let mut runtime = read_runtime()?;

        let desired_names: BTreeSet<String> = state.apps.iter().map(|a| a.name.clone()).collect();
        let runtime_names: Vec<String> = runtime.apps.keys().cloned().collect();
        for name in runtime_names {
            if !desired_names.contains(&name) {
                if let Some(rt) = runtime.apps.remove(&name) {
                    for inst in rt.instances.values() {
                        if inst.pid > 0 {
                            terminate_pid(inst.pid);
                        }
                    }
                }
            }
        }

        for app in state.apps.iter() {
            let rt = runtime.apps.entry(app.name.clone()).or_default();

            let mut remove_idx = Vec::new();
            for (&idx, inst) in rt.instances.iter_mut() {
                if idx >= app.instances {
                    if inst.pid > 0 {
                        terminate_pid(inst.pid);
                    }
                    remove_idx.push(idx);
                    continue;
                }

                if inst.pid > 0 {
                    if let Some(exit) = check_exit_status(inst.pid) {
                        inst.pid = 0;
                        inst.last_exit_at = Some(now_ts());
                        inst.last_exit_code = exit.code;
                        inst.last_exit_signal = exit.signal;
                    } else if !is_pid_alive(inst.pid) {
                        inst.pid = 0;
                        inst.last_exit_at = Some(now_ts());
                        inst.last_exit_code = None;
                        inst.last_exit_signal = None;
                    }
                }
            }
            for idx in remove_idx {
                rt.instances.remove(&idx);
            }

            let desired_instances = if app.paused { 0 } else { app.instances };
            for idx in 0..desired_instances {
                let inst = rt.instances.entry(idx).or_default();
                if inst.pid == 0 {
                    match spawn_instance(app, idx) {
                        Ok(pid) => {
                            if inst.started_at > 0 {
                                inst.restarts += 1;
                            }
                            inst.pid = pid;
                            inst.started_at = now_ts();
                        }
                        Err(err) => {
                            eprintln!("spawn failed for {}[{}]: {err}", app.name, idx);
                        }
                    }
                }
            }
        }

        write_runtime(&runtime)?;
        if watch {
            let snapshot = build_status_snapshot(&state.apps, &runtime);
            render_status_screen_with_selection(&snapshot, 0, 0, false);
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn spawn_instance(app: &AppConfig, idx: usize) -> Result<i32> {
    let log = log_path(&app.name, idx);
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open log {:?}", log))?;

    let mut cmd = Command::new(&app.cmd);
    if app.clean_env {
        cmd.env_clear();
    }
    if let Some(path) = &app.env_file {
        for (k, v) in load_env_file(path)? {
            cmd.env(k, v);
        }
    }
    cmd.args(&app.args);
    cmd.env("RUNNER_APP", &app.name);
    cmd.env("RUNNER_INSTANCE", idx.to_string());
    cmd.env("RUNNER_LOG", log.to_string_lossy().to_string());
    cmd.stdin(Stdio::null());
    cmd.stdout(log_file.try_clone()?);
    cmd.stderr(log_file);

    let child = cmd.spawn().with_context(|| format!("spawn {}", app.cmd))?;
    Ok(child.id() as i32)
}

fn ensure_daemon_running() -> Result<()> {
    if let Some(pid) = read_pid_file()? {
        if is_pid_alive(pid) {
            return Ok(());
        }
    }
    let exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    cmd.spawn().context("spawn daemon")?;
    Ok(())
}

fn base_dir() -> PathBuf {
    if let Some(home) = home::home_dir() {
        home.join(".alwaysrunning")
    } else {
        PathBuf::from(".alwaysrunning")
    }
}

fn ensure_dirs() -> Result<()> {
    fs::create_dir_all(base_dir().join("apps"))?;
    fs::create_dir_all(base_dir().join("run"))?;
    Ok(())
}

fn state_path() -> PathBuf {
    base_dir().join("state.json")
}

fn runtime_path() -> PathBuf {
    base_dir().join("runtime.json")
}

fn pid_path() -> PathBuf {
    base_dir().join("run").join("daemon.pid")
}

fn app_dir(name: &str) -> PathBuf {
    base_dir().join("apps").join(name)
}

fn log_path(name: &str, idx: usize) -> PathBuf {
    app_dir(name).join("logs").join(format!("instance-{}.log", idx))
}

fn read_state() -> Result<State> {
    read_json(&state_path()).or_else(|_| Ok(State::default()))
}

fn read_runtime() -> Result<Runtime> {
    match read_json::<Runtime>(&runtime_path()) {
        Ok(rt) => Ok(rt),
        Err(_) => match read_json::<RuntimeV0>(&runtime_path()) {
            Ok(old) => Ok(Runtime::from(old)),
            Err(_) => Ok(Runtime::default()),
        },
    }
}

fn write_state(state: &State) -> Result<()> {
    write_json_atomic(&state_path(), state)
}

fn write_runtime(runtime: &Runtime) -> Result<()> {
    write_json_atomic(&runtime_path(), runtime)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let mut buf = String::new();
    let mut f = File::open(path).with_context(|| format!("open {:?}", path))?;
    f.read_to_string(&mut buf)?;
    let val = serde_json::from_str(&buf)?;
    Ok(val)
}

fn write_json_atomic<T: Serialize>(path: &Path, val: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let data = serde_json::to_vec_pretty(val)?;
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn write_pid_file() -> Result<()> {
    let pid = std::process::id() as i32;
    if let Some(existing) = read_pid_file()? {
        if existing != pid && is_pid_alive(existing) {
            anyhow::bail!("runner already running with pid {}", existing);
        }
    }
    let path = pid_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, pid.to_string())?;
    Ok(())
}

fn read_pid_file() -> Result<Option<i32>> {
    let path = pid_path();
    if !path.exists() {
        return Ok(None);
    }
    let s = fs::read_to_string(&path)?;
    let pid: i32 = s.trim().parse().unwrap_or(0);
    if pid == 0 {
        return Ok(None);
    }
    Ok(Some(pid))
}

fn follow_file(path: &Path, json: bool) -> Result<()> {
    let mut f = File::open(path)?;
    let mut pos = f.seek(SeekFrom::End(0))?;
    loop {
        let mut buf = String::new();
        let new_pos = f.seek(SeekFrom::End(0))?;
        if new_pos > pos {
            f.seek(SeekFrom::Start(pos))?;
            f.read_to_string(&mut buf)?;
            if json {
                for line in buf.lines() {
                    let obj = serde_json::json!({ "line": line });
                    println!("{obj}");
                }
            } else {
                print!("{buf}");
            }
            pos = new_pos;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn print_tail(path: &Path, lines: usize) -> Result<()> {
    let lines = read_tail_lines(path, lines)?;
    println!("{}", lines.join("\n"));
    Ok(())
}

fn read_tail_lines(path: &Path, lines: usize) -> Result<Vec<String>> {
    if lines == 0 {
        return Ok(Vec::new());
    }
    let mut f = File::open(path)?;
    let mut pos = f.seek(SeekFrom::End(0))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut found = 0usize;
    const CHUNK: usize = 8 * 1024;

    while pos > 0 && found <= lines {
        let read_size = std::cmp::min(CHUNK as u64, pos) as usize;
        pos -= read_size as u64;
        f.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; read_size];
        f.read_exact(&mut chunk)?;
        found += chunk.iter().filter(|&&b| b == b'\n').count();
        let mut new_buf = chunk;
        new_buf.extend_from_slice(&buf);
        buf = new_buf;
    }

    let text = String::from_utf8_lossy(&buf);
    let all_lines: Vec<&str> = text.lines().collect();
    let start = all_lines.len().saturating_sub(lines);
    Ok(all_lines[start..].iter().map(|s| s.to_string()).collect())
}

#[derive(Debug, Default, Clone, Copy)]
struct ExitInfo {
    code: Option<i32>,
    signal: Option<i32>,
}

#[cfg(unix)]
fn check_exit_status(pid: i32) -> Option<ExitInfo> {
    if pid <= 0 {
        return None;
    }
    let mut status: i32 = 0;
    let res = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if res == 0 {
        return None;
    }
    if res < 0 {
        return None;
    }
    let mut info = ExitInfo::default();
    if libc::WIFEXITED(status) {
        info.code = Some(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        info.signal = Some(libc::WTERMSIG(status));
    }
    Some(info)
}

#[cfg(windows)]
fn check_exit_status(pid: i32) -> Option<ExitInfo> {
    if pid <= 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if handle == 0 {
            return None;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        if code == STILL_ACTIVE {
            None
        } else {
            Some(ExitInfo {
                code: Some(code as i32),
                signal: None,
            })
        }
    }
}

#[cfg(all(not(unix), not(windows)))]
fn check_exit_status(_pid: i32) -> Option<ExitInfo> {
    None
}

fn load_env_file(path: &str) -> Result<Vec<(String, String)>> {
    let content = fs::read_to_string(path).with_context(|| format!("read env file {}", path))?;
    let mut out = Vec::new();
    for (idx, raw) in content.lines().enumerate() {
        let mut line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim();
        }
        let Some((key, value)) = line.split_once('=') else {
            anyhow::bail!("Invalid env line {} in {}", idx + 1, path);
        };
        let key = key.trim();
        if key.is_empty() {
            anyhow::bail!("Invalid env line {} in {}", idx + 1, path);
        }
        let mut value = value.trim().to_string();
        if value.len() >= 2 {
            let bytes = value.as_bytes();
            let first = bytes[0] as char;
            let last = bytes[value.len() - 1] as char;
            if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
                value = value[1..value.len() - 1].to_string();
            }
        }
        out.push((key.to_string(), value));
    }
    Ok(out)
}

fn parse_since(input: &str) -> Result<SystemTime> {
    let s = input.trim();
    if s.is_empty() {
        anyhow::bail!("--since is empty");
    }
    if let Ok(ts) = s.parse::<u64>() {
        return Ok(UNIX_EPOCH + Duration::from_secs(ts));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let value: u64 = num
        .parse()
        .with_context(|| format!("invalid --since value: {input}"))?;
    let secs = match unit {
        "s" | "S" => value,
        "m" | "M" => value * 60,
        "h" | "H" => value * 60 * 60,
        "d" | "D" => value * 60 * 60 * 24,
        _ => anyhow::bail!("invalid --since unit: {input} (use s/m/h/d or unix seconds)"),
    };
    Ok(SystemTime::now()
        .checked_sub(Duration::from_secs(secs))
        .unwrap_or(UNIX_EPOCH))
}

#[cfg(test)]
mod tests {
    use super::{load_env_file, Cli, Commands};
    use clap::Parser;
    use std::fs;
    use std::path::PathBuf;

    fn write_temp(contents: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let pid = std::process::id();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        path.push(format!("alwaysrunning-test-{pid}-{ts}.env"));
        fs::write(&path, contents).expect("write temp env file");
        path
    }

    #[test]
    fn parses_basic_env_lines() {
        let path = write_temp(
            r#"
# comment
FOO=bar
export BAZ="hello world"
QUX='quoted'
EMPTY=
"#,
        );
        let pairs = load_env_file(path.to_string_lossy().as_ref()).expect("parse env");
        fs::remove_file(path).ok();

        assert!(pairs.contains(&("FOO".to_string(), "bar".to_string())));
        assert!(pairs.contains(&("BAZ".to_string(), "hello world".to_string())));
        assert!(pairs.contains(&("QUX".to_string(), "quoted".to_string())));
        assert!(pairs.contains(&("EMPTY".to_string(), "".to_string())));
    }

    #[test]
    fn rejects_invalid_lines() {
        let path = write_temp("NO_EQUALS");
        let err = load_env_file(path.to_string_lossy().as_ref()).unwrap_err();
        fs::remove_file(path).ok();
        let msg = err.to_string();
        assert!(msg.contains("Invalid env line"));
    }

    #[test]
    fn preserves_equals_in_value() {
        let path = write_temp("TOKEN=abc=def=ghi");
        let pairs = load_env_file(path.to_string_lossy().as_ref()).expect("parse env");
        fs::remove_file(path).ok();
        assert!(pairs.contains(&("TOKEN".to_string(), "abc=def=ghi".to_string())));
    }

    #[test]
    fn cli_parses_run_args() {
        let cli = Cli::try_parse_from([
            "runner",
            "run",
            "myapp",
            "./bin",
            "--env-file",
            ".env",
            "--clean-env",
            "--foreground",
            "--instances",
            "3",
            "--",
            "--flag",
            "value",
        ])
        .expect("parse");

        match cli.command {
            Commands::Run {
                name,
                cmd,
                env_file,
                clean_env,
                foreground,
                args,
                instances,
                no_autostart,
            } => {
                assert_eq!(name, "myapp");
                assert_eq!(cmd, "./bin");
                assert_eq!(env_file.as_deref(), Some(".env"));
                assert!(clean_env);
                assert!(foreground);
                assert_eq!(instances, 3);
                assert!(!no_autostart);
                assert_eq!(args, vec!["--flag", "value"]);
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn cli_parses_status_watch() {
        let cli = Cli::try_parse_from(["runner", "status", "--watch"]).expect("parse");
        match cli.command {
            Commands::Status { name, watch, json } => {
                assert!(name.is_none());
                assert!(watch);
                assert!(!json);
            }
            _ => panic!("expected status command"),
        }
    }

    #[test]
    fn cli_parses_stop_signal() {
        let cli =
            Cli::try_parse_from(["runner", "stop", "myapp", "--signal", "KILL"]).expect("parse");
        match cli.command {
            Commands::Stop { name, all, signal } => {
                assert_eq!(name.as_deref(), Some("myapp"));
                assert!(!all);
                assert_eq!(signal, "KILL");
            }
            _ => panic!("expected stop command"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn parse_signal_variants() {
        assert_eq!(super::parse_signal("TERM").unwrap(), libc::SIGTERM);
        assert_eq!(super::parse_signal("SIGKILL").unwrap(), libc::SIGKILL);
        assert_eq!(super::parse_signal("2").unwrap(), 2);
    }

    #[test]
    #[cfg(windows)]
    fn parse_signal_variants_windows() {
        assert!(super::parse_signal("TERM").is_ok());
        assert!(super::parse_signal("KILL").is_ok());
    }

    #[test]
    fn parse_since_duration() {
        let since = super::parse_since("10m").expect("since");
        let now = std::time::SystemTime::now();
        let delta = now
            .duration_since(since)
            .unwrap_or_else(|_| std::time::Duration::from_secs(0))
            .as_secs();
        assert!(delta >= 9 * 60 && delta <= 11 * 60);
    }
}

#[cfg(unix)]
fn terminate_pid(pid: i32) {
    terminate_pid_with_signal(pid, libc::SIGTERM);
}

#[cfg(windows)]
fn terminate_pid(pid: i32) {
    terminate_pid_with_signal(pid, 0);
}

#[cfg(all(not(unix), not(windows)))]
fn terminate_pid(_pid: i32) {}

#[cfg(unix)]
fn terminate_pid_with_signal(pid: i32, signal: i32) {
    unsafe {
        libc::kill(pid, signal);
    }
}

#[cfg(windows)]
fn terminate_pid_with_signal(pid: i32, _signal: i32) {
    if pid <= 0 {
        return;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid as u32);
        if handle == 0 {
            return;
        }
        let _ = TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
}

#[cfg(all(not(unix), not(windows)))]
fn terminate_pid_with_signal(_pid: i32, _signal: i32) {}

#[cfg(unix)]
fn terminate_graceful(pid: i32, signal: i32, timeout: Duration) {
    if pid <= 0 {
        return;
    }
    terminate_pid_with_signal(pid, signal);
    if signal == libc::SIGKILL {
        return;
    }
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !is_pid_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    terminate_pid_with_signal(pid, libc::SIGKILL);
}

#[cfg(windows)]
fn terminate_graceful(pid: i32, _signal: i32, _timeout: Duration) {
    terminate_pid_with_signal(pid, 0);
}

#[cfg(all(not(unix), not(windows)))]
fn terminate_graceful(_pid: i32, _signal: i32, _timeout: Duration) {}

fn parse_signal(signal: &str) -> Result<i32> {
    #[cfg(windows)]
    {
        let s = signal.trim().to_uppercase();
        if s.parse::<i32>().is_ok() {
            return Ok(0);
        }
        let s = s.strip_prefix("SIG").unwrap_or(&s);
        match s {
            "TERM" | "KILL" | "INT" | "HUP" | "QUIT" | "USR1" | "USR2" | "STOP" | "CONT" => {
                Ok(0)
            }
            _ => anyhow::bail!("Unknown signal: {signal}"),
        }
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = signal;
        anyhow::bail!("Signals are not supported on this OS.");
    }
    #[cfg(unix)]
    {
        let s = signal.trim().to_uppercase();
        if let Ok(num) = s.parse::<i32>() {
            return Ok(num);
        }
        let s = s.strip_prefix("SIG").unwrap_or(&s);
        let sig = match s {
            "TERM" => libc::SIGTERM,
            "KILL" => libc::SIGKILL,
            "INT" => libc::SIGINT,
            "HUP" => libc::SIGHUP,
            "QUIT" => libc::SIGQUIT,
            "USR1" => libc::SIGUSR1,
            "USR2" => libc::SIGUSR2,
            "STOP" => libc::SIGSTOP,
            "CONT" => libc::SIGCONT,
            _ => anyhow::bail!("Unknown signal: {signal}"),
        };
        Ok(sig)
    }
}

fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    unsafe {
        let res = libc::kill(pid, 0);
        if res == 0 {
            return true;
        }
        let err = last_errno();
        return err == libc::EPERM;
    }
    #[cfg(windows)]
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if handle == 0 {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(unix)]
unsafe fn last_errno() -> i32 {
    #[cfg(target_os = "linux")]
    {
        unsafe { *libc::__errno_location() }
    }
    #[cfg(target_os = "android")]
    {
        unsafe { *libc::__errno_location() }
    }
    #[cfg(target_os = "macos")]
    {
        unsafe { *libc::__error() }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        0
    }
}

#[cfg(unix)]
fn daemonize() -> Result<()> {
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            anyhow::bail!("fork failed");
        }
        if pid > 0 {
            std::process::exit(0);
        }
        if libc::setsid() < 0 {
            anyhow::bail!("setsid failed");
        }
        let pid = libc::fork();
        if pid < 0 {
            anyhow::bail!("fork failed");
        }
        if pid > 0 {
            std::process::exit(0);
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn daemonize() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_launchd() -> Result<()> {
    let exe = std::env::current_exe()?;
    let uid = unsafe { libc::geteuid() };
    let label = "com.alwaysrunning.runner";
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key>\n\
  <string>{label}</string>\n\
  <key>ProgramArguments</key>\n\
  <array>\n\
    <string>{}</string>\n\
    <string>daemon</string>\n\
    <string>--foreground</string>\n\
  </array>\n\
  <key>RunAtLoad</key>\n\
  <true/>\n\
  <key>KeepAlive</key>\n\
  <true/>\n\
  <key>StandardOutPath</key>\n\
  <string>{}</string>\n\
  <key>StandardErrorPath</key>\n\
  <string>{}</string>\n\
</dict>\n\
</plist>\n",
        exe.display(),
        base_dir().join("run/daemon.out").display(),
        base_dir().join("run/daemon.err").display()
    );
    let path = launchd_plist_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, plist)?;
    let target = format!("gui/{uid}");
    let _ = Command::new("launchctl")
        .arg("bootstrap")
        .arg(&target)
        .arg(&path)
        .status();
    let _ = Command::new("launchctl")
        .arg("enable")
        .arg(format!("{target}/{label}"))
        .status();
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_launchd() -> Result<()> {
    let uid = unsafe { libc::geteuid() };
    let label = "com.alwaysrunning.runner";
    let path = launchd_plist_path();
    let target = format!("gui/{uid}");
    let _ = Command::new("launchctl")
        .arg("bootout")
        .arg(&target)
        .arg(&path)
        .status();
    let _ = Command::new("launchctl")
        .arg("disable")
        .arg(format!("{target}/{label}"))
        .status();
    let _ = fs::remove_file(&path);
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchd_plist_path() -> PathBuf {
    home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/LaunchAgents/com.alwaysrunning.runner.plist")
}

#[cfg(target_os = "linux")]
fn install_systemd_user() -> Result<()> {
    let exe = std::env::current_exe()?;
    let unit = format!(
        "[Unit]\nDescription=AlwaysRunning runner daemon\n\n\
[Service]\nExecStart={} daemon --foreground\nRestart=always\n\n\
[Install]\nWantedBy=default.target\n",
        exe.display()
    );
    let path = systemd_unit_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, unit)?;
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "enable", "--now", "alwaysrunning-runner.service"])
        .status();
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_systemd_user() -> Result<()> {
    let path = systemd_unit_path();
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "alwaysrunning-runner.service"])
        .status();
    let _ = fs::remove_file(&path);
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> PathBuf {
    home::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/systemd/user/alwaysrunning-runner.service")
}
