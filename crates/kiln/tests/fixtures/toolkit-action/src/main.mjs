// F-20 (docs/ci.md W06-AC01): every command and file API of the current toolkit, each effect
// observed by a later step, the post step, or the server's events.
import * as core from "@actions/core";
import * as exec from "@actions/exec";
import * as io from "@actions/io";

// getInput: required, a default, trimming and not, booleans, multi-line.
const required = core.getInput("required", { required: true });
const missing = core.getInput("missing");
const padded = core.getInput("padded");
const raw = core.getInput("padded", { trimWhitespace: false });
const flag = core.getBooleanInput("flag");
const lines = core.getMultilineInput("lines");
core.info(`inputs: required=${required} missing=[${missing}] padded=[${padded}] raw=[${raw}] flag=${flag} lines=${JSON.stringify(lines)}`);
try {
  core.getInput("absent", { required: true });
} catch (e) {
  core.info(`required absent: ${e.message}`);
}

core.setOutput("o1", "v1");
core.setOutput("multi", "line one\nline two");
core.exportVariable("EXPORTED", "e1");
core.addPath("/opt/f20-bin");
core.setSecret("super-secret-f20");
core.info("printing the secret: super-secret-f20");
core.saveState("k", "state-v");
core.debug("a debug line");
core.startGroup("G");
core.info("inside the group");
core.endGroup();
core.notice("a notice", { title: "N", file: "README.md", startLine: 1 });
core.warning("a warning", { file: "README.md", startLine: 2, endLine: 3, startColumn: 1, endColumn: 4 });
core.error(new Error("an error object"));
await core.summary.addHeading("F-20").addTable([[{ data: "name", header: true }, { data: "value", header: true }], ["o1", "v1"]]).write();
await exec.exec("echo", ["from exec"]);
const out = await exec.getExecOutput("sh", ["-c", "printf captured"]);
core.info(`exec output: ${out.stdout} (exit ${out.exitCode})`);
await io.mkdirP(`${process.env.RUNNER_TEMP}/f20/a/b`);
core.info(`which sh: ${(await io.which("sh", true)) !== ""}`);
core.info(`is debug: ${core.isDebug()}`);
// getIDToken (W10): a token for an audience, masked once fetched.
if (process.env.ACTIONS_ID_TOKEN_REQUEST_URL) {
  const token = await core.getIDToken("kiln-f20");
  const claims = JSON.parse(Buffer.from(token.split(".")[1], "base64url").toString());
  core.info(`id token: aud=${claims.aud} sub=${claims.sub}`);
  core.info(`the token itself: ${token}`);
}
