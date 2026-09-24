//! The client children's command line, built as plain data so what each
//! child is told can be checked without spawning one.

use super::*;

/// The argument vector for one client child: `count` clients starting at
/// global id `offset`, aimed at the served server on `server_port`, with
/// `workers` runtime workers. Every knob the CLIENT side reads must be
/// forwarded here — a knob the orchestrator forwards only to the server
/// leaves the two processes disagreeing about the run.
pub(super) fn client_args(
    args: &Args,
    count: u64,
    offset: u64,
    server_port: u16,
    workers: usize,
) -> Vec<String> {
    let mut cargs = vec![
        count.to_string(),
        "--addr".into(),
        format!("127.0.0.1:{server_port}"),
        "--offset".into(),
        offset.to_string(),
        "--duration".into(),
        args.duration.as_secs().to_string(),
        "--move-ms".into(),
        args.move_ms.as_millis().to_string(),
        "--room".into(),
        args.room.to_string(),
        "--stagger-ms".into(),
        args.stagger_ms.to_string(),
        "--profile".into(),
        match args.profile {
            Profile::Ring => "ring".into(),
            Profile::Spread => "spread".into(),
            Profile::Still => "still".into(),
        },
        "--still-frac".into(),
        args.still_frac.to_string(),
        "--spawn-half-size".into(),
        args.spawn_half.to_string(),
        // The client view recomputes spatial cells from wire coordinates
        // (`CellExit` eviction), so it needs the server's cell size.
        "--cell-size".into(),
        args.cell_size.to_string(),
        "--transport".into(),
        args.transport.to_string(),
        "--workers".into(),
        workers.to_string(),
    ];
    // The flood client (by global id) belongs to exactly one child:
    // forward the flag only to the child whose id range contains it.
    if let Some(k) = args.flood_id
        && offset <= k
        && k < offset + count
    {
        cargs.push("--flood-id".into());
        cargs.push(k.to_string());
    }
    if let Some(c) = args.churn_secs {
        cargs.push("--churn-secs".into());
        cargs.push(c.to_string());
    }
    if args.churn_cycles != 0 {
        cargs.push("--churn-cycles".into());
        cargs.push(args.churn_cycles.to_string());
    }
    if let Some(f) = args.disconnect_grace_secs {
        cargs.push("--disconnect-grace-secs".into());
        cargs.push(f.to_string());
    }
    cargs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value that follows `flag` in an argument vector, if present.
    fn value_of<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        argv.iter()
            .position(|a| a == flag)
            .and_then(|i| argv.get(i + 1))
            .map(String::as_str)
    }

    /// GAME-MODULE §6 decision 10: the client view's cell size must be the
    /// one the server runs with. The client decodes a spatial room's
    /// `CellExit` records by recomputing cells from wire coordinates
    /// (`client/view.rs`), so a child left at the default 20 while the
    /// server runs `--cell-size 50` evicts the wrong entities.
    #[test]
    fn client_children_get_the_orchestrated_cell_size() {
        let mut args = Args::defaults();
        args.cell_size = 50.0;
        let argv = client_args(&args, 10, 0, 7777, 1);
        assert_eq!(value_of(&argv, "--cell-size"), Some("50"));
    }

    /// The default is forwarded too (explicitly, not by omission), so a
    /// child never depends on the two binaries sharing one default.
    #[test]
    fn client_children_get_the_default_cell_size_explicitly() {
        let argv = client_args(&Args::defaults(), 10, 0, 7777, 1);
        assert_eq!(value_of(&argv, "--cell-size"), Some("20"));
    }
}
