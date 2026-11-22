#!/usr/bin/env python3
"""What is left in the engine's production code that breaks CLAUDE.md §1, counted lint by lint from
cargo's JSON messages (stdin) against the counts the ratchet file holds for this host (what is
compiled differs by platform): a count above its line fails, and one below it fails until the line
is lowered, so the file always says what is left. A host the file holds no counts for is counted,
not checked.

  cargo clippy -p amphora-engine ... --message-format=json -- -W clippy::unwrap_used ... |
      python3 scripts/engine-ratchet.py scripts/engine-ratchet.toml <host> clippy::unwrap_used ...
"""

import json
import sys
import tomllib


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__, file=sys.stderr)
        return 2
    path, host, lints = sys.argv[1], sys.argv[2], sys.argv[3:]
    with open(path, "rb") as f:
        held = tomllib.load(f).get("left", {}).get(host)
    seen: dict[str, int] = {lint: 0 for lint in lints}
    places: set[tuple[str, str, int, int]] = set()
    for line in sys.stdin:
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if m.get("reason") != "compiler-message":
            continue
        msg = m.get("message") or {}
        if msg.get("level") == "error":
            # The JSON is read here, not shown: an error would otherwise go unseen.
            print(msg.get("rendered") or msg.get("message"), file=sys.stderr)
            continue
        code = (msg.get("code") or {}).get("code")
        if code not in seen:
            continue
        span = next((s for s in msg.get("spans", []) if s.get("is_primary")), None)
        if span is None or not span.get("file_name", "").startswith("crates/engine"):
            continue
        # One per place: a lint met twice at one place (two targets of a crate) counts once.
        key = (code, span["file_name"], span["line_start"], span["column_start"])
        if key not in places:
            places.add(key)
            seen[code] += 1
    if held is None:
        for lint in lints:
            print(f"engine-ratchet: {lint}: {seen[lint]}")
        print(f"engine-ratchet: {path} holds no counts for {host}: counted, not checked")
        return 0
    failed = False
    for lint in lints:
        was, now = held.get(lint, 0), seen[lint]
        if now > was:
            print(f"engine-ratchet: {lint}: {now}, above the {was} {path} holds: new code follows the rule (CLAUDE.md §1)")
            failed = True
        elif now < was:
            print(f"engine-ratchet: {lint}: {now}, below the {was} {path} holds: lower it to {now}")
            failed = True
    print(f"engine-ratchet: {sum(seen.values())} left in the engine's production code across {len(lints)} lints" + ("" if failed else ", as the ratchet holds"))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
