"""Light SQLite schema/statement fixture; it does not execute Rust or authenticate peers."""
import json
import pathlib
import re
import sqlite3

ROOT = pathlib.Path(__file__).resolve().parents[4]
MIGRATION = ROOT / "crates/kiln/src/store/migrations/20261111-0100_agent_mail.sql"
SOURCE = ROOT / "crates/kiln/src/harness/mail/handoffs.rs"
sql = re.findall(r'tx\.execute\("([^"\n]+)"', SOURCE.read_text())
audit = next(statement for statement in sql if statement.startswith("INSERT INTO agent_mail_transfers"))
rebind = next(statement for statement in sql if statement.startswith("UPDATE agent_mail SET epoch="))
finish = next(statement for statement in sql if statement.startswith("UPDATE agent_mail_handoffs SET"))


def manifest(attempt, source, destination):
    return dict(attempt=attempt, session="session", source_epoch=source, destination_epoch=destination, count=7, sha256="ab" * 32)


def record(value, phase):
    def principal(machine):
        return dict(peer="aa" * 32, machine=machine, instance=machine + "-instance")
    return dict(manifest=value, source=principal(str(value["source_epoch"])), destination=principal(str(value["destination_epoch"])), phase=phase)


def offer(connection, value, ordinal):
    connection.execute(
        "INSERT INTO agent_mail_handoffs(attempt,ordinal,session,source_epoch,destination_epoch,phase,record) VALUES(?,?,?,?,?,'offered',?)",
        (value["attempt"], ordinal, "session", value["source_epoch"], value["destination_epoch"], json.dumps(record(value, "offered"))),
    )


def adopt(connection, value):
    connection.execute("SAVEPOINT handoff")
    try:
        arguments = ("session", value["source_epoch"], value["destination_epoch"], json.dumps(dict(disposition="transferred", epoch=value["destination_epoch"]), separators=(",", ":")))
        connection.execute(audit, arguments)
        connection.execute(rebind, arguments[:3])
        connection.execute(finish, (value["attempt"], "committed", json.dumps(record(value, "committed")), "prepared"))
    except sqlite3.Error:
        connection.execute("ROLLBACK TO handoff")
        connection.execute("RELEASE handoff")
        raise
    connection.execute("RELEASE handoff")


def prepare(connection, value):
    changed = connection.execute(finish, (value["attempt"], "prepared", json.dumps(record(value, "prepared")), "offered")).rowcount
    assert changed == 1


connection = sqlite3.connect(":memory:")
connection.executescript(MIGRATION.read_text())
body = '{"private":"unchanged UTF-8 日本語"}'
for state in ("dispatched", "admitted", "consumed", "cancelled"):
    connection.execute(
        "INSERT INTO agent_mail(owner,sender,recipient,id,body,bytes,disposition,generation,epoch,frozen,terminal,source_connection) VALUES('owner','sender','session',?,?,?, ?,1,1,1,?,'A')",
        (state, body, len(body.encode()), state, "terminal-" + state),
    )
before = connection.execute("SELECT revision,id,body,bytes,disposition,generation,frozen,terminal FROM agent_mail ORDER BY revision").fetchall()
ab, bc = manifest("A-B", 1, 2), manifest("B-C", 2, 3)
offer(connection, ab, 1)
try:
    offer(connection, manifest("competing", 1, 4), 2)
except sqlite3.IntegrityError:
    pass
else:
    raise AssertionError("two active offers accepted")
prepare(connection, ab)
assert connection.execute("SELECT DISTINCT epoch FROM agent_mail").fetchall() == [(1,)]
try:
    offer(connection, manifest("prepared-competing", 1, 4), 2)
except sqlite3.IntegrityError:
    pass
else:
    raise AssertionError("Prepared did not retain its active session fence")
adopt(connection, ab)
offer(connection, bc, 2)
prepare(connection, bc)
adopt(connection, bc)
assert connection.execute("SELECT revision,id,body,bytes,disposition,generation,frozen,terminal FROM agent_mail ORDER BY revision").fetchall() == before
assert set(connection.execute("SELECT DISTINCT epoch,source_connection FROM agent_mail").fetchall()) == {(3, None), (1, "A")}
assert connection.execute("SELECT epoch,source_connection FROM agent_mail WHERE disposition IN ('consumed','cancelled')").fetchall() == [(1, "A"), (1, "A")]
assert connection.execute("SELECT count(*) FROM agent_mail_transfers").fetchone() == (4,)
history = connection.execute("SELECT cursor,generation,from_epoch,to_epoch FROM agent_mail_transfers WHERE revision=1 ORDER BY cursor").fetchall()
assert [(row[1], row[2], row[3]) for row in history] == [(1, 1, 2), (1, 2, 3)]
assert history[1][0] > history[0][0]
assert connection.execute("SELECT generation FROM agent_mail_transfers WHERE revision=1 AND cursor>? ORDER BY cursor", (history[0][0],)).fetchall() == [(1,)]
cd = manifest("C-D-refused", 3, 4)
offer(connection, cd, 3)
prepare(connection, cd)
connection.execute("CREATE TRIGGER refuse_commit BEFORE UPDATE ON agent_mail_handoffs WHEN NEW.phase='committed' BEGIN SELECT RAISE(ABORT,'fixture'); END")
try:
    adopt(connection, cd)
except sqlite3.IntegrityError:
    pass
else:
    raise AssertionError("injected phase failure did not refuse commit")
assert set(connection.execute("SELECT DISTINCT epoch FROM agent_mail").fetchall()) == {(1,), (3,)}
assert connection.execute("SELECT count(*) FROM agent_mail_transfers").fetchone() == (4,)
assert connection.execute("SELECT phase FROM agent_mail_handoffs WHERE attempt='C-D-refused'").fetchone() == ("prepared",)
print("PASS SQLite", sqlite3.sqlite_version, "same-token unresolved A->B->C; terminal history unchanged; Offered/Prepared fence; opaque cursor; rollback after rebind")
connection.close()
