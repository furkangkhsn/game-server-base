//! The command line: every knob the generator takes, and the usage
//! text that documents them.

use crate::*;
use std::time::Duration;

impl Args {
    /// Every knob at its default (what a bare `gsb-loadgen` runs with):
    /// the starting point [`parse_args`] overrides, and the fixture the
    /// command-line builders' tests start from.
    pub(crate) fn defaults() -> Self {
        Self {
            game: gsb_server::games::demo::DemoModule::NAME,
            clients: 100,
            offset: 0,
            duration: Duration::from_secs(10),
            move_ms: Duration::from_millis(150),
            room: 1,
            stagger_ms: 0.0,
            addr: None,
            profile: Profile::Ring,
            still_frac: 0.9,
            spawn_half: 50.0,
            visibility: gsb_server::Visibility::default(),
            topology: None,
            shard_count: 4,
            cell_size: 20.0,
            vision_radius: gsb_demo::team::DEFAULT_VISION_RADIUS,
            max_snapshot_bytes: 1400,
            server_spawn_half: 50.0,
            serve: false,
            bind: "127.0.0.1:7777".into(),
            metrics_listen: None,
            orchestrate: false,
            transport: crate::Transport::Tcp,
            tls_ca: None,
            tls_server_name: "localhost".into(),
            procs: 1,
            pin: false,
            pin_server_cores: 8,
            workers: 0,
            max_players: None,
            max_connections: None,
            idle_timeout_secs: None,
            write_stall_secs: None,
            conn_out: None,
            listen_backlog: None,
            flood_id: None,
            churn_secs: None,
            churn_cycles: 0,
            disconnect_grace_secs: None,
            mmo_duel_frac: 0.0,
            mmo_crystallize: None,
            capture: None,
            capture_clients: 8,
            stall_ms: None,
            stall_every_ms: 5000,
            rpc_rate: None,
            rpc_burst: None,
        }
    }
}

/// A command line the generator refuses: `main` prints it on stderr and
/// exits with status 2 (a usage error) — no panic, no backtrace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliError(pub(crate) String);

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

/// What the command line asks for.
pub(crate) enum Cli {
    /// A run (whichever mode its flags pick).
    Run(Box<Args>),
    /// `-h` / `--help`: print the usage text and exit successfully.
    Help,
}

/// `flag`'s value `s` as a number.
fn number<T: std::str::FromStr>(flag: &str, s: String) -> Result<T, CliError> {
    s.parse()
        .map_err(|_| CliError(format!("{flag}: expected a number, got `{s}`")))
}

/// `Err(why)` unless `ok`.
fn refuse_unless(ok: bool, why: &str) -> Result<(), CliError> {
    if ok {
        Ok(())
    } else {
        Err(CliError(why.to_string()))
    }
}

/// The process's own command line (see [`parse`]).
pub(crate) fn parse_args() -> Result<Cli, CliError> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    parse(&argv)
}

/// Parse `argv` (the arguments after the program name) into a run or a
/// help request, or the reason the line cannot run.
pub(crate) fn parse(argv: &[String]) -> Result<Cli, CliError> {
    let mut args = Args::defaults();
    // The game-specific flags written, checked against `--game` once the
    // whole line is read (the flags may come in any order).
    let mut game_flags: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].clone();
        i += 1;
        if crate::bot::is_game_only(&a) {
            game_flags.push(a.clone());
        }
        let mut v = || {
            if i < argv.len() {
                let s = argv[i].clone();
                i += 1;
                Ok(s)
            } else {
                Err(CliError(format!("{a} needs a value (try --help)")))
            }
        };
        match a.as_str() {
            "-h" | "--help" => return Ok(Cli::Help),
            "--game" => args.game = crate::bot::game_named(&v()?).map_err(CliError)?,
            "--duration" => args.duration = Duration::from_secs_f64(number(&a, v()?)?),
            "--move-ms" => args.move_ms = Duration::from_millis(number(&a, v()?)?),
            "--room" => args.room = number(&a, v()?)?,
            "--offset" => args.offset = number(&a, v()?)?,
            "--stagger-ms" => args.stagger_ms = number(&a, v()?)?,
            "--addr" => args.addr = Some(v()?),
            "--profile" => {
                args.profile = Profile::parse(&v()?).ok_or_else(|| {
                    CliError("--profile: expected ring|spread|still (try --help)".into())
                })?
            }
            "--still-frac" => {
                let f: f64 = number(&a, v()?)?;
                refuse_unless((0.0..=1.0).contains(&f), "--still-frac must be in 0..=1")?;
                args.still_frac = f;
            }
            "--spawn-half-size" => {
                let s = number(&a, v()?)?;
                args.spawn_half = s;
                args.server_spawn_half = s; // one knob: spawn map = home map
            }
            "--visibility" => {
                let s = v()?;
                args.visibility = match s.as_str() {
                    "all" => gsb_server::Visibility::All,
                    "spatial" => gsb_server::Visibility::Spatial,
                    "team" => gsb_server::Visibility::Team,
                    "pvs" => gsb_server::Visibility::Pvs,
                    "sharded" => gsb_server::Visibility::Sharded,
                    other => {
                        return Err(CliError(format!(
                            "--visibility: expected all|spatial|team|pvs|sharded, got {other}"
                        )));
                    }
                };
            }
            "--topology" => {
                let s = v()?;
                args.topology = match s.as_str() {
                    "single" => Some(gsb_server::Topology::Single),
                    "sharded" => Some(gsb_server::Topology::Sharded),
                    other => {
                        return Err(CliError(format!(
                            "--topology: expected single|sharded, got {other}"
                        )));
                    }
                };
            }
            "--shard-count" => {
                args.shard_count = number(&a, v()?)?;
            }
            "--cell-size" => args.cell_size = number(&a, v()?)?,
            "--vision-radius" => args.vision_radius = number(&a, v()?)?,
            "--max-snapshot-bytes" => args.max_snapshot_bytes = number(&a, v()?)?,
            "--serve" => args.serve = true,
            "--bind" => args.bind = v()?,
            "--metrics-listen" => args.metrics_listen = Some(v()?),
            "--orchestrate" => args.orchestrate = true,
            "--procs" => args.procs = number(&a, v()?)?,
            "--pin" => args.pin = true,
            "--pin-server-cores" => args.pin_server_cores = number(&a, v()?)?,
            "--workers" => args.workers = number(&a, v()?)?,
            "--max-players" => {
                let n: u32 = number(&a, v()?)?;
                args.max_players = Some(n);
            }
            "--max-connections" => {
                let n: u64 = number(&a, v()?)?;
                args.max_connections = Some(n);
            }
            "--idle-timeout-secs" => {
                args.idle_timeout_secs = Some(number(&a, v()?)?);
            }
            "--write-stall-secs" => {
                args.write_stall_secs = Some(number(&a, v()?)?);
            }
            "--conn-out" => {
                let n: usize = number(&a, v()?)?;
                refuse_unless(n >= 1, "--conn-out must be at least 1")?;
                args.conn_out = Some(n);
            }
            "--listen-backlog" => {
                let n: u32 = number(&a, v()?)?;
                refuse_unless(
                    gsb_net::listen::listen_backlog_problem(n).is_none(),
                    "--listen-backlog must be 1..=2147483647 (the kernel caps it at somaxconn)",
                )?;
                args.listen_backlog = Some(n);
            }
            "--flood-id" => args.flood_id = Some(number(&a, v()?)?),
            "--churn-secs" => {
                let f: f64 = number(&a, v()?)?;
                refuse_unless(f > 0.0, "--churn-secs must be > 0")?;
                args.churn_secs = Some(f);
            }
            "--disconnect-grace-secs" => {
                args.disconnect_grace_secs = Some(number(&a, v()?)?);
            }
            "--mmo-duel-frac" => {
                let f: f64 = number(&a, v()?)?;
                refuse_unless((0.0..=1.0).contains(&f), "--mmo-duel-frac must be in 0..=1")?;
                args.mmo_duel_frac = f;
            }
            "--mmo-crystallize" => {
                args.mmo_crystallize = match v()?.as_str() {
                    "on" => Some(true),
                    "off" => Some(false),
                    other => {
                        return Err(CliError(format!(
                            "--mmo-crystallize: expected on|off, got {other}"
                        )));
                    }
                };
            }
            "--churn-cycles" => {
                args.churn_cycles = number(&a, v()?)?;
            }
            "--transport" => {
                let s = v()?;
                args.transport = crate::Transport::parse(&s).ok_or_else(|| {
                    CliError(format!("--transport: expected tcp|udp|ws, got {s}"))
                })?;
            }
            "--capture" => args.capture = Some(v()?),
            "--capture-clients" => {
                let k: u64 = number(&a, v()?)?;
                refuse_unless(k > 0, "--capture-clients must be > 0")?;
                args.capture_clients = k;
            }
            "--stall-ms" => {
                let ms: u64 = number(&a, v()?)?;
                refuse_unless(ms > 0, "--stall-ms must be > 0")?;
                args.stall_ms = Some(ms);
            }
            "--stall-every-ms" => {
                let ms: u64 = number(&a, v()?)?;
                refuse_unless(ms > 0, "--stall-every-ms must be > 0")?;
                args.stall_every_ms = ms;
            }
            "--rpc-rate" => {
                let r: f64 = number(&a, v()?)?;
                refuse_unless(
                    r > 0.0 && r.is_finite(),
                    "--rpc-rate must be > 0 (requests/s per client)",
                )?;
                args.rpc_rate = Some(r);
            }
            "--rpc-burst" => {
                let b: u32 = number(&a, v()?)?;
                refuse_unless(b >= 1, "--rpc-burst must be at least 1")?;
                args.rpc_burst = Some(b);
            }
            "--tls-ca" => args.tls_ca = Some(v()?),
            "--tls-server-name" => args.tls_server_name = v()?,
            s if s.starts_with("--") => {
                return Err(CliError(format!("unknown flag {s} (try --help)")));
            }
            s => args.clients = number("N", s.to_string())?,
        }
    }
    check::check(&mut args, &game_flags)?;
    Ok(Cli::Run(Box::new(args)))
}

mod check;
#[cfg(test)]
mod tests;
