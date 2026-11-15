import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentfileSource } from "../src/api/agentfiles.ts";
import { formOf, lineOf, pinWords, problemOf, writeSource } from "../src/views/agentfiles/sources.ts";

const HEX = "9f2c55d0b7a1e4c8f3d6a9b2e5c8f1a4d7b0e3c6f9a2d5b8e1c4f7a0d3b6e9c2";
const src = (s: Partial<AgentfileSource>): AgentfileSource => ({ line: 1, directive: "AGENT", name: "x", kind: "path", source: "./x", pin: null, credential: null, grant: null, ...s });

test("a pin in a few characters: a digest's first 8, an object version's, a ref and its commit, kiln's own", () => {
  assert.equal(pinWords({ by: "sha256", value: HEX, resolved: null }), "sha256 9f2c55d0");
  assert.equal(pinWords({ by: "version-id", value: "3HL4kqtJlcpXroDTDmJ.rmSpXd3dIbrHY", resolved: null }), "ver 3HL4kqtJ");
  assert.equal(pinWords({ by: "ref", value: "v4", resolved: "4701b59d9934" }), "v4 → 4701b59d");
  assert.equal(pinWords({ by: "kiln", value: "", resolved: null }), "kiln's pin");
  assert.equal(pinWords(null), "unpinned");
});

test("a source's line is the one that names it now, else where it was when kept", () => {
  const text = "FROM rust\nAGENT reviewer FROM ./r\nAGENT reviewer-two FROM ./r2\n";
  assert.equal(lineOf(text, { directive: "AGENT", name: "reviewer-two", line: 9 }), 3);
  assert.equal(lineOf(text, { directive: "AGENT", name: "reviewer", line: 9 }), 2);
  assert.equal(lineOf(text, { directive: "HARNESS", name: "reviewer", line: 9 }), 9);
});

test("the picker opens on a line as written: a Git ref with its folder, an S3 version and region, an https SHA-256", () => {
  assert.deepEqual(formOf(src({ kind: "git-ssh" }), "AGENT reviewer FROM git@github.com:hyper-light/agents.git#v4:reviewer"), {
    kind: "git",
    source: "git@github.com:hyper-light/agents.git",
    pin: "v4:reviewer",
    region: "",
  });
  assert.deepEqual(formOf(src({ kind: "s3" }), "AGENT triage --version-id=3HL4 --region=us-east-1 FROM s3://b/k.tar.gz"), { kind: "s3", source: "s3://b/k.tar.gz", pin: "3HL4", region: "us-east-1" });
  assert.deepEqual(formOf(src({ kind: "https" }), `HARNESS codex --checksum=sha256:${HEX} FROM https://x.dev/c.tar.gz`), { kind: "https", source: "https://x.dev/c.tar.gz", pin: HEX, region: "" });
});

test("nothing is written that Shards would refuse: an https source needs its SHA-256, S3 a key and a pin, Git a .git URL and a ref", () => {
  assert.equal(problemOf({ kind: "https", source: "https://x.dev/c.tar.gz", pin: "abc", region: "" }), "64 hex digits");
  assert.equal(problemOf({ kind: "https", source: "https://x.dev/r.git", pin: HEX, region: "" }), "an https URL");
  assert.equal(problemOf({ kind: "s3", source: "s3://bucket", pin: "v1", region: "" }), "s3://bucket/key");
  assert.equal(problemOf({ kind: "s3", source: "s3://b/k", pin: "", region: "" }), "a version or SHA-256");
  assert.equal(problemOf({ kind: "git", source: "https://github.com/a/b", pin: "v1", region: "" }), "a .git URL");
  assert.equal(problemOf({ kind: "git", source: "git@github.com:a/b.git", pin: "", region: "" }), "a ref");
  assert.equal(problemOf({ kind: "path", source: "agents/x", pin: "", region: "" }), "./ or /");
  assert.equal(problemOf({ kind: "path", source: "./agents/x", pin: "", region: "" }), null);
});

test("a line written again in Shards' syntax: its own flags replaced, any other kept, the rest of the line as it was", () => {
  // Git: the ref after #, and --checksum dropped (Shards pins a Git source by its ref alone).
  assert.equal(
    writeSource(`  AGENT reviewer --checksum=sha256:${HEX} --redirect=objects.example:443 FROM ./r`, { kind: "git", source: "https://github.com/a/b.git", pin: "v5:dir", region: "" }),
    "  AGENT reviewer --redirect=objects.example:443 FROM https://github.com/a/b.git#v5:dir",
  );
  // https: --checksum=sha256:, with or without its prefix typed.
  assert.equal(
    writeSource("HARNESS claude FROM ./harness", { kind: "https", source: "https://x.dev/c.tar.gz", pin: `sha256:${HEX}`, region: "" }),
    `HARNESS claude --checksum=sha256:${HEX} FROM https://x.dev/c.tar.gz`,
  );
  // S3: an object version or a SHA-256, its region, and --endpoint kept as written.
  assert.equal(
    writeSource("AGENT t --endpoint=https://r2.example --version-id=old --region=eu-west-1 FROM s3://b/old", { kind: "s3", source: "s3://b/new.tar.gz", pin: "NEW", region: "us-east-1" }),
    "AGENT t --version-id=NEW --region=us-east-1 --endpoint=https://r2.example FROM s3://b/new.tar.gz",
  );
  assert.equal(writeSource("AGENT t FROM s3://b/k", { kind: "s3", source: "s3://b/k", pin: HEX, region: "" }), `AGENT t --checksum=sha256:${HEX} FROM s3://b/k`);
  // A path takes no pin of its own.
  assert.equal(writeSource(`AGENT t --checksum=sha256:${HEX} FROM https://x.dev/t.tgz`, { kind: "path", source: "./agents/t", pin: "", region: "" }), "AGENT t FROM ./agents/t");
  // Not a source line, or a form Shards would refuse: nothing.
  assert.equal(writeSource("FROM rust:1.91", { kind: "path", source: "./x", pin: "", region: "" }), null);
  assert.equal(writeSource("AGENT t FROM ./t", { kind: "https", source: "https://x.dev/t.tgz", pin: "short", region: "" }), null);
});
