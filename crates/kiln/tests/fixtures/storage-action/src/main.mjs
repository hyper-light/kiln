// W08-AC01 (docs/ci.md): the published cache and artifact clients against kiln's runtime services,
// one operation a step, each saying what it saw.
import * as core from "@actions/core";
import * as cache from "@actions/cache";
import { DefaultArtifactClient } from "@actions/artifact";
import * as fs from "node:fs";
import * as path from "node:path";

const op = core.getInput("op", { required: true });
const key = core.getInput("key");
const name = core.getInput("name");
const artifacts = new DefaultArtifactClient();
const files = (root) => fs.readdirSync(root, { recursive: true }).map((f) => path.join(root, f)).filter((f) => fs.statSync(f).isFile());

switch (op) {
  case "save": {
    // -1 when the key exists already: the client warns and goes on.
    const id = await cache.saveCache(core.getMultilineInput("paths"), key);
    core.info(`saved ${key}: ${id}`);
    break;
  }
  case "restore": {
    const hit = await cache.restoreCache(core.getMultilineInput("paths"), key, core.getMultilineInput("restore-keys"));
    core.info(`restored ${key}: ${hit ?? "miss"}`);
    break;
  }
  case "upload": {
    const root = core.getInput("root");
    const list = files(root);
    const r = await artifacts.uploadArtifact(name, list, root, { compressionLevel: 0 });
    core.info(`uploaded ${name}: ${list.length} files, ${r.size} bytes, id ${r.id}`);
    core.setOutput("id", String(r.id));
    break;
  }
  case "list": {
    const { artifacts: all } = await artifacts.listArtifacts();
    core.info(`listed: ${all.map((a) => a.name).sort().join(",")}`);
    break;
  }
  case "download": {
    const by = core.getInput("id");
    const to = core.getInput("to");
    const id = by ? Number(by) : (await artifacts.getArtifact(name)).artifact.id;
    const r = await artifacts.downloadArtifact(id, { path: to });
    const got = files(r.downloadPath ?? to);
    const bytes = got.reduce((n, f) => n + fs.statSync(f).size, 0);
    core.info(`downloaded by ${by ? "id" : "name"}: ${got.length} files, ${bytes} bytes`);
    break;
  }
  case "delete": {
    const r = await artifacts.deleteArtifact(name);
    core.info(`deleted ${name}: ${r.id > 0}`);
    break;
  }
  default:
    core.setFailed(`no op ${op}`);
}
