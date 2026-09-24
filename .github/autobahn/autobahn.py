#!/usr/bin/env python3
"""Autobahn TestSuite gate for the gsb WebSocket door (CI job `autobahn`).

One file owns the whole policy, so the exclusions and their reasons
cannot drift apart:

  autobahn.py spec  OUT.json    write the fuzzingclient spec
  autobahn.py check INDEX.json  judge the report; exit 1 on any surprise

The target is `examples/ws_autobahn.rs` (gsb-net): the production WS door
with the opaque message mapping, so arbitrary binary payloads echo back.

Every case runs except the ones the gsb wire contract refuses on
purpose (EXCLUDED, each with its reason). Of the cases that run, each
must report OK or INFORMATIONAL for both the behavior and the close
behavior, unless ACCEPTED names it with a reason. A NON-STRICT, FAILED,
UNCLEAN or WRONG CODE result anywhere else fails the build, and so does
a report that is missing a group we rely on or that ran an excluded case.
"""

import json
import re
import sys

AGENT = "gsb"
SERVER_URL = "ws://127.0.0.1:9001"

TEXT = ("text messages are not part of the gsb wire contract: the door refuses "
        "them with 1003 (pinned by ws::tests::fragmentation and "
        "ws::tests::protocol), so a case that needs a text echo or a text "
        "UTF-8 verdict cannot pass by design")

# pattern -> reason. Patterns use Autobahn's own syntax ('*' is a wildcard,
# everything else literal, matched against the whole case id).
EXCLUDED = {
    "1.1.*": TEXT + " (text echo, 0-65536 bytes)",
    "3.2": TEXT + " (a text echo precedes the RSV frame)",
    "3.3": TEXT + " (same, frame-wise chops)",
    "3.4": TEXT + " (same, octet-wise chops)",
    "4.1.3": TEXT + " (a text echo precedes the reserved opcode)",
    "4.1.4": TEXT + " (same)",
    "4.1.5": TEXT + " (same)",
    "4.2.3": TEXT + " (a text echo precedes the reserved control opcode)",
    "4.2.4": TEXT + " (same)",
    "4.2.5": TEXT + " (same)",
    "5.3": TEXT + " (fragmented text echo)",
    "5.4": TEXT + " (same, frame-wise chops)",
    "5.5": TEXT + " (same, octet-wise chops)",
    "5.6": TEXT + " (fragmented text with a ping between)",
    "5.7": TEXT + " (same, frame-wise chops)",
    "5.8": TEXT + " (same, octet-wise chops)",
    "5.15": TEXT + " (opens with a fragmented text message, which gets 1003)",
    "5.18": TEXT + " (the first text frame gets 1003 before the second can "
            "interleave; the binary analogue is pinned by "
            "ws::tests::fragmentation)",
    "5.19": TEXT + " (fragmented text with pings between)",
    "5.20": TEXT + " (same, frame-wise chops)",
    "6.*": TEXT + " (the whole UTF-8 group validates text payloads)",
    "7.1.1": TEXT + " (a text echo before the close)",
    "7.1.5": TEXT + " (a text fragment before the close)",
    "7.1.6": TEXT + " (a 256 KiB text message before the close)",
    "9.1.*": TEXT + " (large text messages)",
    "9.3.*": TEXT + " (fragmented large text messages)",
    "9.5.*": TEXT + " (chopped large text messages)",
    "9.7.*": TEXT + " (text round-trip timing)",
    "10.*": TEXT + " (10.1.1 is an auto-fragmented text echo)",
    "12.*": "permessage-deflate is not offered by the door (no extension is "
            "negotiated, so RSV1 stays a 1002); the group would only report "
            "UNIMPLEMENTED",
    "13.*": "same as 12.*: compression is not implemented",
}

# case id -> (allowed behaviors, reason). A non-OK verdict is accepted only
# here, one case at a time, and only with a reason a reviewer can check.
ACCEPTED = {}

# One case from every group the gate is meant to cover: a spec or report
# that silently lost a group must fail, not pass with fewer cases.
REQUIRED = [
    "1.2.1", "1.2.8",  # binary echo, incl. chopped
    "2.1", "2.5", "2.11",  # ping/pong, oversized ping, chopped pings
    "3.1", "3.5", "3.7",  # reserved bits
    "4.1.1", "4.2.1",  # reserved opcodes
    "5.1", "5.9", "5.17",  # fragmented control, orphan continuations
    "7.1.2", "7.3.2", "7.3.6", "7.5.1", "7.7.1", "7.9.1",  # close handling
    "9.2.1", "9.4.1", "9.6.1", "9.8.1",  # large / fragmented / chopped binary
]

PASSING = {"OK", "INFORMATIONAL"}


def pattern_re(pattern):
    return re.compile("^" + ".*".join(map(re.escape, pattern.split("*"))) + "$")


EXCLUDED_RES = [(p, pattern_re(p)) for p in EXCLUDED]


def excluded_by(case_id):
    return next((p for p, rx in EXCLUDED_RES if rx.match(case_id)), None)


def spec():
    return {
        "outdir": "/autobahn/reports",
        "servers": [{"agent": AGENT, "url": SERVER_URL}],
        "cases": ["*"],
        "exclude-cases": list(EXCLUDED),
        "exclude-agent-cases": {},
        # A failed case is judged by its close code, not by who dropped
        # the TCP connection first: the door closes the socket right after
        # its close frame on a protocol failure.
        "options": {"failByDrop": False},
    }


def case_key(case_id):
    return [int(part) for part in case_id.split(".")]


def check(index):
    problems = []
    if AGENT not in index:
        return [f"no results for agent {AGENT!r} (agents: {sorted(index)})"]
    results = index[AGENT]
    for case_id in REQUIRED:
        if case_id not in results:
            problems.append(f"{case_id}: required case did not run")
    for case_id in sorted(results, key=case_key):
        r = results[case_id]
        rule = excluded_by(case_id)
        if rule is not None:
            problems.append(f"{case_id}: ran although {rule!r} excludes it")
            continue
        verdict = (r.get("behavior"), r.get("behaviorClose"))
        allowed, _reason = ACCEPTED.get(case_id, (PASSING, None))
        if verdict[0] not in allowed or verdict[1] not in PASSING | set(allowed):
            problems.append(
                f"{case_id}: behavior={verdict[0]} close={verdict[1]} "
                f"remoteCloseCode={r.get('remoteCloseCode')} ({r.get('reportfile')})"
            )
    ran = len(results)
    print(f"autobahn: {ran} cases ran for {AGENT}, "
          f"{len(EXCLUDED)} exclusion patterns, {len(ACCEPTED)} accepted exceptions")
    return problems


def main(argv):
    if len(argv) == 3 and argv[1] == "spec":
        with open(argv[2], "w") as out:
            json.dump(spec(), out, indent=2)
        return 0
    if len(argv) == 3 and argv[1] == "check":
        with open(argv[2]) as f:
            problems = check(json.load(f))
        for p in problems:
            print("UNEXPECTED " + p)
        print("autobahn: " + ("FAILED" if problems else "all run cases passed"))
        return 1 if problems else 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
