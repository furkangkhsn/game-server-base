#!/usr/bin/env bash
# rudp-jitter.sh — B104: can `udp_congestion` default to "pace"?
#
# Runs the same rUDP load under `--udp-congestion off` and `pace` while a
# netem qdisc on the loopback interface shapes the path, and prints one
# table: per scenario and mode, the pacing episodes and rate cuts, the
# game-band frames the pacing queued and dropped, the loss the clients
# reported, the mean probe RTT, connect p99, the snapshot rate and the
# sessions that ended under their clients.
#
# The question has two halves, and the scenarios answer them in turn:
#   1. jitter WITHOUT a bottleneck must not trigger pacing (no episodes, no
#      rate cuts, nothing dropped or held back) — or pacing as a default
#      would throttle healthy but jittery players;
#   2. a REAL bottleneck must still trigger it (episodes, cuts, frames
#      dropped by the pacing instead of by the path), with jitter or not.
#
# WARNING
#   * netem on `lo` shapes ALL loopback traffic on this machine while the
#     script runs (databases, IDEs, browsers' local servers, ...). Run it
#     on an otherwise quiet machine; nothing else should be loading the
#     CPU either — a starved process looks like jitter too.
#   * It needs root for `tc` and calls `sudo tc ...` itself (sudo may ask
#     for a password). It refuses to start if `lo` already has a root
#     qdisc other than the default `noqueue`, and it removes the qdisc it
#     added on every exit (normal end, error, Ctrl-C, TERM).
#   * netem applies on egress, and a loopback datagram leaves `lo` once
#     per direction: the delays below are PER DIRECTION, the round trip
#     sees them twice. netem jitter also reorders datagrams (a later one
#     may draw a shorter delay) — harsher than most real paths.
#
# Scenarios (delay mean ± jitter per direction, `distribution normal`):
#   baseline            no netem: the machine's own floor.
#   jitter20            20 ms ± 10 ms  (~40 ms RTT): good broadband / Wi-Fi.
#   jitter40            40 ms ± 20 ms  (~80 ms RTT): typical 4G/LTE,
#                       cross-country.
#   jitter60            60 ms ± 40 ms  (~120 ms RTT, σ near the mean):
#                       congested Wi-Fi or cellular — the case most likely
#                       to read as a standing queue without being one.
#   jitter40_loss1      jitter40 + 1 % random loss: a lossy radio link;
#                       random loss alone must not count as congestion.
#   bottleneck          `rate` = BOTTLENECK_FRAC (default 0.5) × the
#                       baseline run's measured loopback demand (server in
#                       + out bytes/s), so the path carries about half of
#                       what the room sends: a real, sustained bottleneck
#                       with a netem queue of BOTTLENECK_LIMIT packets.
#                       (The server counts frame bytes; netem counts whole
#                       packets, headers included — the real demand is
#                       larger, so the derived rate is at most half of it.)
#   bottleneck_jitter   the same rate with jitter20 on top: the bottleneck
#                       must still be seen through the jitter.
#   The jitter steps are spaced so a false trigger shows where it starts;
#   ±40 ms at 60 ms keeps most draws positive (netem clips at zero).
#
# Environment (defaults in brackets):
#   CLIENTS [64]          clients per run (one room: the demo's).
#   DURATION [30]         seconds per run — several probe intervals, and
#                         room for an episode to start (two signals in a
#                         row) and recover.
#   RUNS [3]              runs per scenario and mode; the modes alternate
#                         (off,pace / pace,off) so drift hits both alike.
#   SCENARIOS [all]       space-separated subset of the names above.
#   LOADGEN_MODE [orchestrate]   orchestrate (server and clients in their
#                         own processes) or inproc.
#   LOADGEN_EXTRA []      extra gsb-loadgen arguments (e.g. --game arena).
#   BOTTLENECK_RATE []    a fixed tc rate (e.g. 4mbit) instead of the
#                         derived one; required when baseline is skipped.
#   BOTTLENECK_FRAC [0.5] BOTTLENECK_LIMIT [1000]
#   OUT_DIR [./target/rudp-jitter/<timestamp>]  where every run's output,
#                         results.tsv and summary.txt go.
#   RUDP_JITTER_DRY [0]   1 = skip every sudo/tc call (the plumbing and the
#                         summary only; every scenario runs unshaped).
#
# Usage: scripts/rudp-jitter.sh        (from anywhere inside the repo)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLIENTS="${CLIENTS:-64}"
DURATION="${DURATION:-30}"
RUNS="${RUNS:-3}"
LOADGEN_MODE="${LOADGEN_MODE:-orchestrate}"
LOADGEN_EXTRA="${LOADGEN_EXTRA:-}"
BOTTLENECK_RATE="${BOTTLENECK_RATE:-}"
BOTTLENECK_FRAC="${BOTTLENECK_FRAC:-0.5}"
BOTTLENECK_LIMIT="${BOTTLENECK_LIMIT:-1000}"
DRY="${RUDP_JITTER_DRY:-0}"
ALL_SCENARIOS="baseline jitter20 jitter40 jitter60 jitter40_loss1 bottleneck bottleneck_jitter"
SCENARIOS="${SCENARIOS:-$ALL_SCENARIOS}"
OUT_DIR="${OUT_DIR:-$ROOT/target/rudp-jitter/$(date +%Y%m%d-%H%M%S)}"
IFACE=lo

die() { echo "rudp-jitter: $*" >&2; exit 1; }
say() { echo "rudp-jitter: $*" >&2; }

for n in CLIENTS DURATION RUNS BOTTLENECK_LIMIT; do
    [[ "${!n}" =~ ^[1-9][0-9]*$ ]] || die "$n must be a positive integer (got '${!n}')"
done
[[ "$LOADGEN_MODE" == orchestrate || "$LOADGEN_MODE" == inproc ]] \
    || die "LOADGEN_MODE must be orchestrate or inproc"
for s in $SCENARIOS; do
    [[ " $ALL_SCENARIOS " == *" $s "* ]] || die "unknown scenario '$s' (known: $ALL_SCENARIOS)"
done

# --- the qdisc: refuse a foreign one, remove ours on every exit ---------
QDISC_OURS=0

tc_run() {
    if [[ "$DRY" == 1 ]]; then
        say "dry: sudo tc $*"
    else
        sudo tc "$@"
    fi
}

clear_qdisc() {
    if [[ "$QDISC_OURS" == 1 ]]; then
        QDISC_OURS=0
        if [[ "$DRY" == 1 ]]; then
            say "dry: sudo tc qdisc del dev $IFACE root"
        elif ! sudo -n tc qdisc del dev "$IFACE" root 2>/dev/null \
            && ! sudo tc qdisc del dev "$IFACE" root; then
            say "WARNING: could not remove the netem qdisc on $IFACE —" \
                "remove it by hand: sudo tc qdisc del dev $IFACE root"
        fi
    fi
}

trap 'clear_qdisc' EXIT
trap 'clear_qdisc; exit 130' INT
trap 'clear_qdisc; exit 143' TERM

if [[ "$DRY" != 1 ]]; then
    command -v tc >/dev/null || die "tc (iproute2) is not installed"
    command -v sudo >/dev/null || die "sudo is not available"
    sudo -v || die "sudo refused: tc needs root"
    current="$(tc qdisc show dev "$IFACE")" || die "cannot read the qdiscs of $IFACE"
    if grep -q ' root ' <<<"$current" && ! grep -q '^qdisc noqueue 0: root' <<<"$current"; then
        die "$IFACE already has a root qdisc ($current) — not touching it; remove it first"
    fi
fi

# Apply one netem configuration (none for the baseline).
apply_qdisc() {
    clear_qdisc
    if [[ $# -gt 0 ]]; then
        [[ "$DRY" == 1 ]] || sudo -v
        tc_run qdisc add dev "$IFACE" root netem "$@"
        QDISC_OURS=1
    fi
}

# --- the build: once, release -------------------------------------------
say "building gsb-loadgen (release)"
(cd "$ROOT" && cargo build --release -q -p gsb-server --bin gsb-loadgen)
LOADGEN="${CARGO_TARGET_DIR:-$ROOT/target}/release/gsb-loadgen"
[[ -x "$LOADGEN" ]] || die "no binary at $LOADGEN"

mkdir -p "$OUT_DIR"
RESULTS="$OUT_DIR/results.tsv"
: >"$RESULTS"
{
    echo "clients=$CLIENTS duration=$DURATION runs=$RUNS mode=$LOADGEN_MODE extra=$LOADGEN_EXTRA"
    echo "dry=$DRY started=$(date -Is) host=$(uname -n) kernel=$(uname -r)"
} >"$OUT_DIR/run.txt"

# The netem arguments of a scenario.
netem_args() {
    local rate="$1"
    case "$2" in
        baseline) ;;
        jitter20) echo "delay 20ms 10ms distribution normal" ;;
        jitter40) echo "delay 40ms 20ms distribution normal" ;;
        jitter60) echo "delay 60ms 40ms distribution normal" ;;
        jitter40_loss1) echo "delay 40ms 20ms distribution normal loss 1%" ;;
        bottleneck) echo "rate $rate limit $BOTTLENECK_LIMIT" ;;
        bottleneck_jitter) echo "delay 20ms 10ms distribution normal rate $rate limit $BOTTLENECK_LIMIT" ;;
    esac
}

# One loadgen run; its RESULT line goes to results.tsv.
run_one() {
    local scenario="$1" mode="$2" run="$3" log
    log="$OUT_DIR/$scenario/$mode-$run.log"
    mkdir -p "$OUT_DIR/$scenario"
    local -a argv=("$CLIENTS")
    [[ "$LOADGEN_MODE" == orchestrate ]] && argv=(--orchestrate "$CLIENTS")
    # shellcheck disable=SC2206 # LOADGEN_EXTRA is word-split on purpose
    argv+=(--duration "$DURATION" --transport udp --udp-congestion "$mode" $LOADGEN_EXTRA)
    say "$scenario $mode run $run/$RUNS"
    if ! timeout $((DURATION + 180)) "$LOADGEN" "${argv[@]}" >"$log" 2>&1; then
        say "WARNING: $scenario $mode run $run exited non-zero (see $log)"
    fi
    local line
    line="$(grep -m1 '^RESULT ' "$log" || true)"
    [[ -n "$line" ]] || line="RESULT missing=1"
    printf '%s\t%s\t%s\t%s\n' "$scenario" "$mode" "$run" "$line" >>"$RESULTS"
}

# A RESULT key's value from results.tsv's first row of a scenario.
first_value() {
    awk -F'\t' -v s="$1" -v k="$2" '$1 == s {
        n = split($4, kv, " ")
        for (i = 1; i <= n; i++) if (index(kv[i], k "=") == 1) { print substr(kv[i], length(k) + 2); exit }
    }' "$RESULTS"
}

rate=""
for scenario in $SCENARIOS; do
    if [[ "$scenario" == bottleneck* && -z "$rate" ]]; then
        if [[ -n "$BOTTLENECK_RATE" ]]; then
            rate="$BOTTLENECK_RATE"
        else
            sin="$(first_value baseline server_in_bps)"
            sout="$(first_value baseline server_out_bps)"
            [[ -n "$sin" && -n "$sout" ]] \
                || die "no baseline to derive the bottleneck from: run baseline first or set BOTTLENECK_RATE"
            rate="$(awk -v i="$sin" -v o="$sout" -v f="$BOTTLENECK_FRAC" \
                'BEGIN { r = int((i + o) * 8 * f / 1000); if (r < 64) r = 64; print r "kbit" }')"
        fi
        say "bottleneck rate: $rate (limit $BOTTLENECK_LIMIT packets)"
        echo "bottleneck_rate=$rate limit=$BOTTLENECK_LIMIT" >>"$OUT_DIR/run.txt"
    fi
    # shellcheck disable=SC2046 # the netem arguments are word-split on purpose
    apply_qdisc $(netem_args "$rate" "$scenario")
    for ((r = 1; r <= RUNS; r++)); do
        if ((r % 2)); then modes="off pace"; else modes="pace off"; fi
        for mode in $modes; do
            run_one "$scenario" "$mode" "$r"
        done
    done
    clear_qdisc
done

# --- the summary: per scenario × mode, means per run --------------------
awk -F'\t' '
function v(k,    i) { return (k in kv) ? kv[k] : 0 }
{
    key = $1 SUBSEP $2
    if (!(key in runs)) { order[++nk] = key }
    runs[key]++
    delete kv
    n = split($4, f, " ")
    for (i = 2; i <= n; i++) { p = index(f[i], "="); if (p) kv[substr(f[i], 1, p - 1)] = substr(f[i], p + 1) }
    if ("missing" in kv) { missing[key]++; next }
    ep[key] += v("transport_udp_game_paced_episodes")
    cut[key] += v("transport_udp_game_paced_rate_cuts")
    drop[key] += v("transport_udp_game_frames_dropped_paced")
    que[key] += v("transport_udp_game_frames_queued_paced")
    lost[key] += v("transport_udp_game_datagrams_reported_lost")
    sent[key] += v("transport_udp_game_datagrams_reported_sent")
    rtt[key] += v("transport_udp_game_rtt_sum_us")
    rtn[key] += v("transport_udp_game_rtt_samples")
    p99[key] += v("connect_p99_ms")
    if (v("connect_p99_ms") > p99max[key]) p99max[key] = v("connect_p99_ms")
    snap[key] += v("snap_per_s")
    ends[key] += v("udp_ends_rel_dead") + v("udp_ends_reset") + v("udp_ends_seal_limit")
    ok[key]++
}
END {
    printf "%-18s %-4s %4s %8s %8s %9s %9s %7s %8s %9s %9s %10s %5s\n", \
        "scenario", "mode", "runs", "episodes", "cuts", "dropped", "queued", "lost%", \
        "rtt_ms", "conn_p99", "p99_max", "snaps/s", "ends"
    for (j = 1; j <= nk; j++) {
        key = order[j]; split(key, sm, SUBSEP); k = ok[key]
        if (k == 0) { printf "%-18s %-4s %4d  (no RESULT line)\n", sm[1], sm[2], runs[key]; continue }
        printf "%-18s %-4s %4s %8.1f %8.1f %9.1f %9.1f %7.2f %8.1f %9.1f %9d %10.1f %5d\n", \
            sm[1], sm[2], k (missing[key] ? "!" : ""), ep[key] / k, cut[key] / k, \
            drop[key] / k, que[key] / k, sent[key] ? 100 * lost[key] / sent[key] : 0, \
            rtn[key] ? rtt[key] / rtn[key] / 1000 : 0, p99[key] / k, p99max[key], \
            snap[key] / k, ends[key]
    }
    print ""
    print "episodes, cuts, dropped, queued, snaps/s and conn_p99 are means per run;"
    print "lost% = reported lost / reported sent over all runs; rtt_ms = mean probe"
    print "RTT over all samples; p99_max = the worst run; ends = sessions that"
    print "ended under their clients, all runs (B128). A run count marked ! had runs"
    print "without a RESULT line (see their logs)."
}' "$RESULTS" | tee "$OUT_DIR/summary.txt"

say "done: $OUT_DIR"
