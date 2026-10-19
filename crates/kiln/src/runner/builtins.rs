//! kiln's built-in actions (docs/ci.md W07-R10): `uses: kiln/<name>@v1`, run by the runner itself
//! (no Node, nothing to fetch), in either dialect, since `kiln/*` resolves to these before any
//! remote repository. Their metadata is served as `action.yml` would be, by
//! `GET /api/ci/actions/kiln`.

use serde_json::{Value, json};

/// An input of a built-in.
pub struct Input {
    pub name: &'static str,
    pub description: &'static str,
    pub default: Option<&'static str>,
    pub required: bool,
}

/// A built-in action.
pub struct Builtin {
    pub name: &'static str,
    pub description: &'static str,
    pub inputs: &'static [Input],
    pub outputs: &'static [(&'static str, &'static str)],
}

const fn input(name: &'static str, description: &'static str, default: Option<&'static str>) -> Input {
    Input { name, description, default, required: false }
}

/// The built-ins this runner has.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "upload",
        description: "Upload an artifact (W08): the files zipped as `actions/upload-artifact@v4` keeps them, so `actions/download-artifact` reads it too, uploaded by the runner itself (no Node), its SHA-256 checked by the server. The files' root is the deepest directory every path is under.",
        inputs: &[
            input("name", "The artifact's name.", Some("artifact")),
            Input { name: "path", description: "Files, directories or patterns, one a line, from the workspace; `!` excludes.", default: None, required: true },
            input("if-no-files-found", "`warn`, `error` or `ignore`.", Some("warn")),
            input("retention-days", "Days to keep it (0: the server's default).", Some("0")),
            input("compression-level", "0 (stored) to 9.", Some("6")),
            input("overwrite", "Replace one of the same name in this run.", Some("false")),
            input("include-hidden-files", "Include files and directories whose names start with `.`.", Some("false")),
        ],
        outputs: &[
            ("artifact-id", "The artifact's id."),
            ("artifact-url", "Where it's downloaded from."),
            ("artifact-digest", "Its zip's SHA-256."),
        ],
    },
    Builtin {
        name: "download",
        description: "Download a run's artifacts (W08), its own jobs' or `actions/upload-artifact`'s, by the runner itself (no Node), each checked against the SHA-256 it was uploaded with.",
        inputs: &[
            input("name", "One artifact, by name; all of the run's when left out.", None),
            input("pattern", "Without `name`: those whose names match (`*`, `?`).", None),
            input("path", "Where to put them, from the workspace (several: each in a directory of its name).", None),
            input("merge-multiple", "Several into `path` itself.", Some("false")),
        ],
        outputs: &[("download-path", "Where they went.")],
    },
    Builtin {
        name: "checkout",
        description: "Check out the run's commit (or another ref) from the server's mirror: shallow by default, sparse and partial when asked.",
        inputs: &[
            input("path", "Where, below the workspace.", Some(".")),
            input("ref", "A commit, branch or tag; the run's commit by default.", None),
            input("fetch-depth", "Commits of history; 0 for all of it.", Some("1")),
            input("sparse-checkout", "Patterns (one a line) to check out only those paths.", None),
            input("sparse-checkout-cone-mode", "Read the patterns as directories (git's cone mode).", Some("true")),
            input("filter", "A partial clone filter, such as `blob:none`.", None),
        ],
        outputs: &[("ref", "The ref checked out."), ("commit", "The commit checked out.")],
    },
    Builtin {
        name: "test-results",
        description: "Send test reports to kiln, which keeps and parses them: results on the run, failures annotated where they are, quarantine and flakiness judged (W16).",
        inputs: &[
            Input { name: "path", description: "The reports: patterns, one a line, from the working directory.", default: None, required: true },
            input("format", "junit, tap, go-test-json, libtest-json, trx, xunit, mocha-json or ctrf; found from each file when left out.", None),
            input("fail-on-failure", "Fail the step when a test failed (quarantined ones, under `quarantine: enforce`, excepted).", Some("true")),
        ],
        outputs: &[("failed", "How many tests failed."), ("passed", "How many passed.")],
    },
    Builtin {
        name: "auth",
        description: "Authenticate the steps after this one to an integration (W11-R03): AWS, Google Cloud and Azure through their own federation files and variables, with the leg's ID token (refreshed as the leg runs); other kinds' stored tokens as the variables their tools read.",
        inputs: &[Input { name: "integration", description: "The integration, by name.", default: None, required: true }],
        outputs: &[("kind", "The integration's kind."), ("env", "The variables set, comma-separated.")],
    },
    Builtin {
        name: "build-image",
        description: "Build a runner image (W13-R11) from `.kiln/images/<name>/Dockerfile` (docker buildx, multi-platform where the builder can) or an Agentfile (shards build), push it to kiln's registry with the job's token, and register it for pools and routing rules.",
        inputs: &[
            Input { name: "name", description: "The image's name: `.kiln/images/<name>/`.", default: None, required: true },
            input("context", "The build's directory (`.kiln/images/<name>` by default).", None),
            input("file", "The Dockerfile (`<context>/Dockerfile` by default).", None),
            input("platforms", "Platforms, comma-separated (`linux/amd64,linux/arm64`); the runner's own by default.", None),
            input("tag", "The tag (the commit's first twelve characters by default; `latest` too).", None),
        ],
        outputs: &[("image", "The image: `<registry>/<repository>@<digest>`."), ("digest", "Its digest.")],
    },
    Builtin {
        name: "publish",
        description: "Publish a package to its registry (W12-R04) with an integration's credentials: npm (`npm publish`), PyPI (`uv publish`, else twine), crates.io (`cargo publish`), NuGet (`dotnet nuget push`), RubyGems (`gem push`), Maven (`mvn deploy`). A private registry (Artifactory, Nexus, GitHub Packages, GitLab Packages, CodeArtifact, Azure Artifacts, Artifact Registry, Cloudsmith) is the integration's `registry`.",
        inputs: &[
            Input { name: "ecosystem", description: "npm, pypi, crates, nuget, rubygems or maven.", default: None, required: true },
            input("integration", "The registry's credentials: an integration of the leg's, by name (its token, and its `registry` URL).", None),
            input("path", "What to publish: the package's directory (npm, cargo, maven), its files (a pattern: dist/* for PyPI, *.nupkg, *.gem).", Some(".")),
            input("registry", "The registry's URL, when not the integration's or the ecosystem's public one.", None),
        ],
        outputs: &[("registry", "Where it went.")],
    },
    Builtin {
        name: "attest",
        description: "Attest what the job built (W18): SLSA v1.0 provenance the server makes from its own records of the run, which the job's steps can't write, and an SBOM of the working directory's lockfiles when asked, signed by the server as Sigstore bundles (beside each file as `<file>.sigstore.json`). kiln's way to attest: `actions/attest-build-provenance` works only with a Fulcio that trusts kiln's OIDC issuer.",
        inputs: &[
            input("subject-path", "Files: patterns, one a line, from the working directory.", None),
            input("subject-name", "A subject named by hand, with `subject-digest`.", None),
            input("subject-digest", "That subject's digest: `sha256:<hex>`.", None),
            input("subject-checksums", "A `sha256sum` list (a file): each line a subject.", None),
            input("subject-image", "Images, one a line: their digests from the runner's container engine, or a reference's own `@sha256:`.", None),
            input("subjects", "kiln's: files or images, one a line (as a job's `attest.subjects`).", None),
            input("provenance", "Attest SLSA v1.0 provenance.", Some("true")),
            input("sbom", "Also attest an SBOM: `spdx` (or `true`), `cyclonedx`, or `false`.", Some("false")),
        ],
        outputs: &[
            ("bundle-path", "The provenance's Sigstore bundle (the SBOM's, when it is the only one)."),
            ("sbom-path", "The SBOM."),
            ("sbom-bundle-path", "The SBOM attestation's bundle."),
            ("attestation-id", "The provenance's id."),
            ("attestation-url", "Where the run lists its attestations."),
        ],
    },
    Builtin {
        name: "notify",
        description: "Send a message through the repository's channels: Slack, Discord, Teams, Telegram, PagerDuty, Opsgenie or a webhook (W19).",
        inputs: &[
            Input { name: "to", description: "Channels, one a line: `slack:#ci`, `discord:team-hook`, `webhook:audit`, …", default: None, required: true },
            Input { name: "message", description: "What to say.", default: None, required: true },
        ],
        outputs: &[("sent", "How many channels took it.")],
    },
];

/// `kiln/<name>@<version>`: the built-in it names, when it is one (`v1`, or any version
/// starting `v1`).
pub fn builtin(uses: &str) -> Option<&'static Builtin> {
    let rest = uses.strip_prefix("kiln/")?;
    let (name, version) = rest.split_once('@')?;
    if !(version == "v1" || version.starts_with("v1.")) {
        return None;
    }
    BUILTINS.iter().find(|b| b.name == name)
}

/// Whether `uses` is kiln's own namespace (`kiln/<name>@…`), built in or not: never fetched from
/// a remote repository.
pub fn is_kiln(uses: &str) -> bool {
    uses.strip_prefix("kiln/").is_some_and(|r| !r.contains('/') && r.contains('@'))
}

/// The built-ins as `action.yml` reads, for the catalog and the editor (W07-R12).
pub fn catalog() -> Value {
    Value::Array(
        BUILTINS
            .iter()
            .map(|b| {
                let inputs: serde_json::Map<String, Value> = b.inputs.iter().map(|i| (i.name.to_owned(), json!({ "description": i.description, "required": i.required, "default": i.default }))).collect();
                let outputs: serde_json::Map<String, Value> = b.outputs.iter().map(|(n, d)| ((*n).to_owned(), json!({ "description": d }))).collect();
                json!({ "uses": format!("kiln/{}@v1", b.name), "name": b.name, "description": b.description, "inputs": inputs, "outputs": outputs, "runs": { "using": "kiln" } })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w07_r10_names() {
        assert_eq!(builtin("kiln/checkout@v1").map(|b| b.name), Some("checkout"));
        assert_eq!(builtin("kiln/test-results@v1.2").map(|b| b.name), Some("test-results"));
        assert!(builtin("kiln/checkout@v2").is_none() && builtin("kiln/nope@v1").is_none() && builtin("kilnx/checkout@v1").is_none());
        assert!(is_kiln("kiln/nope@v1") && !is_kiln("kiln/repo/path@v1") && !is_kiln("actions/checkout@v4"));
        assert!(catalog().as_array().is_some_and(|a| a.len() == BUILTINS.len()));
    }
}
