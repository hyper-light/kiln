//! Providers (docs/cd-targets.md §2, §3): every platform kiln deploys to, shown to the engine as
//! a cluster of objects.
//!
//! - **Targets are clusters.** A target is a provider, the account (an integration) it acts
//!   through, and a place: `aws-ecs` in `us-east-1` through `aws-prod`. The engine knows it as a
//!   cluster named `aws-ecs/aws-prod/us-east-1` ([`Target`]), so everything it does for clusters
//!   (diffs, plans, waves and hooks, history and rollback, self-heal, shadow mode) it does for
//!   every target.
//! - **Objects keep Kubernetes' shape** (§1.2): `apiVersion: ecs.aws.kiln.dev/v1`, `kind:
//!   Service`, `metadata.name`, `metadata.namespace` for the provider's scope (an ECS cluster),
//!   `spec` in the platform's own API field names, `status` as the platform reports it.
//!
//! This is each provider's pure half: its kinds, an app's definition rendered into objects, the
//! platform's answers read into objects, the requests a change makes, and health. The I/O half
//! (the platform's API, credentials, polling) is the server's (`kiln::cd::targets`).

pub mod aws_ecs;
pub mod aws_lambda;
pub mod cloudflare_workers;
pub mod vercel;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::health::Status;

/// What a provider can do (§2.1). The UI offers only that.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// Weighted rollout steps (§5).
    Traffic,
    /// Immutable versions: a rollback moves traffic and uploads nothing.
    Versions,
    /// Its own preview URLs per diff, stack or pull request.
    Previews,
    /// Logs, for the object panel.
    Logs,
    /// Events, for the object panel.
    Events,
    /// Outside edits are readable (otherwise the UI says drift isn't visible).
    Drift,
    /// Discovery, for import and shadow mode.
    Adopt,
    /// The platform checks a change without making it (a dry run).
    Validate,
}

/// One kind of object a provider shows the engine.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct KindDef {
    pub group: &'static str,
    pub kind: &'static str,
    /// Whether it lives in the provider's scope (`metadata.namespace`: an ECS cluster), or in the
    /// target itself.
    pub namespaced: bool,
    /// What the platform calls it: "Task definition".
    pub title: &'static str,
    /// Holds data: kept when its app prunes or goes, unless a person confirms (`Delete=false`).
    pub stateful: bool,
    /// Only read (a version, a task): kiln shows it in the tree and never applies it.
    pub child: bool,
    /// Carries traffic between versions: a rollout moves it (docs/cd-targets.md §5).
    pub traffic: bool,
}

const fn kind(group: &'static str, kind: &'static str, title: &'static str) -> KindDef {
    KindDef { group, kind, namespaced: false, title, stateful: false, child: false, traffic: false }
}

const fn traffic(k: KindDef) -> KindDef {
    KindDef { traffic: true, ..k }
}

const fn scoped(k: KindDef) -> KindDef {
    KindDef { namespaced: true, ..k }
}

const fn stateful(k: KindDef) -> KindDef {
    KindDef { stateful: true, ..k }
}

const fn child(k: KindDef) -> KindDef {
    KindDef { child: true, ..k }
}

/// One setting of an app's definition for a provider (its key in `.kiln/deploy.yaml`).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Setting {
    pub name: &'static str,
    pub required: bool,
    pub help: &'static str,
}

const fn s(name: &'static str, required: bool, help: &'static str) -> Setting {
    Setting { name, required, help }
}

/// A provider (§3).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ProviderDef {
    pub name: &'static str,
    pub title: &'static str,
    /// The integration kind its accounts are (`aws`, `cloudflare`), or `runner` for targets kiln's
    /// runners reach.
    pub integration: &'static str,
    /// What names a place, in order: `region`; `project` and `region`.
    pub place: &'static [&'static str],
    /// The definition's setting that names the provider's scope (`cluster`, for an ECS cluster):
    /// the app's destination namespace.
    pub scope: Option<&'static str>,
    /// An app's definition: its key in `.kiln/deploy.yaml`.
    pub settings: &'static [Setting],
    pub kinds: &'static [KindDef],
    pub capabilities: &'static [Capability],
    /// The platform's own files it reads, so an app moves to kiln without rewriting (§4).
    pub reads: &'static [&'static str],
    /// Rendering, health and the platform's API are built; otherwise it is listed, not usable.
    pub built: bool,
}

use Capability::*;

/// Every provider in docs/cd-targets.md §3 (D2: all of them; this is the order they are built
/// in, not a choice of which).
pub const CATALOG: &[ProviderDef] = &[
    ProviderDef {
        name: "aws-ecs",
        title: "Amazon ECS",
        integration: "aws",
        place: &["region"],
        scope: Some("cluster"),
        settings: &[
            s("cluster", true, "the ECS cluster"),
            s("service", true, "the service's name"),
            s("task", true, "the task definition: a JSON file in the source (RegisterTaskDefinition's input), or an object"),
            s("service-spec", false, "the service's settings: a JSON file or an object (CreateService's input: desiredCount, networkConfiguration, loadBalancers, …)"),
            s("set", false, "values to set by path: {\"containerDefinitions[name=api].image\": \"${{ artifacts.image }}\"}"),
        ],
        kinds: &[kind("ecs.aws.kiln.dev", "TaskDefinition", "Task definition"), scoped(kind("ecs.aws.kiln.dev", "Service", "Service")), child(scoped(kind("ecs.aws.kiln.dev", "Task", "Task")))],
        capabilities: &[Versions, Events, Logs, Drift, Adopt],
        reads: &["task definition JSON"],
        built: true,
    },
    ProviderDef {
        name: "aws-lambda",
        title: "AWS Lambda",
        integration: "aws",
        place: &["region"],
        scope: None,
        settings: &[
            s("function", true, "the function's name"),
            s("alias", false, "the alias kiln moves to each version it publishes (live)"),
            s("code", true, "the code: s3://bucket/key of a zip, or an image URI (${{ artifacts.* }})"),
            s("config", false, "the function's configuration: a JSON file or an object (CreateFunction's input: Runtime, Handler, Role, MemorySize, Environment, …)"),
            s("set", false, "values to set by path: {\"Environment.Variables.MODE\": \"prod\"}"),
            s("url", false, "a function URL: {AuthType: NONE | AWS_IAM, Cors}"),
        ],
        kinds: &[
            kind("lambda.aws.kiln.dev", "Function", "Function"),
            traffic(kind("lambda.aws.kiln.dev", "Alias", "Alias")),
            kind("lambda.aws.kiln.dev", "FunctionUrl", "Function URL"),
            child(kind("lambda.aws.kiln.dev", "Version", "Version")),
        ],
        capabilities: &[Traffic, Versions, Logs, Drift, Adopt],
        reads: &["SAM template.yaml"],
        built: true,
    },
    ProviderDef {
        name: "aws-beanstalk",
        title: "AWS Elastic Beanstalk",
        integration: "aws",
        place: &["region"],
        scope: None,
        settings: &[s("application", true, "the application"), s("environment", true, "the environment"), s("bundle", true, "the source bundle (${{ artifacts.* }})")],
        kinds: &[kind("elasticbeanstalk.aws.kiln.dev", "Application", "Application"), kind("elasticbeanstalk.aws.kiln.dev", "ApplicationVersion", "Application version"), kind("elasticbeanstalk.aws.kiln.dev", "Environment", "Environment")],
        capabilities: &[Traffic, Versions, Events, Logs, Drift, Adopt],
        reads: &["Dockerrun.aws.json", ".ebextensions"],
        built: false,
    },
    ProviderDef {
        name: "aws-ec2",
        title: "Amazon EC2 Auto Scaling",
        integration: "aws",
        place: &["region"],
        scope: None,
        settings: &[s("group", true, "the Auto Scaling group"), s("launch-template", true, "the launch template"), s("image", false, "the AMI (${{ artifacts.* }})")],
        kinds: &[kind("ec2.aws.kiln.dev", "LaunchTemplate", "Launch template"), kind("autoscaling.aws.kiln.dev", "AutoScalingGroup", "Auto Scaling group"), child(kind("ec2.aws.kiln.dev", "Instance", "Instance"))],
        capabilities: &[Events, Drift, Adopt],
        reads: &[],
        built: false,
    },
    ProviderDef {
        name: "aws-static",
        title: "Amazon S3 and CloudFront",
        integration: "aws",
        place: &["region"],
        scope: None,
        settings: &[s("bucket", true, "the bucket"), s("prefix", false, "the prefix sites go under"), s("distribution", false, "the CloudFront distribution to switch and invalidate"), s("site", true, "the built site (${{ artifacts.* }})")],
        kinds: &[kind("s3.aws.kiln.dev", "Site", "Site"), kind("cloudfront.aws.kiln.dev", "Distribution", "Distribution")],
        capabilities: &[Versions, Drift],
        reads: &[],
        built: false,
    },
    ProviderDef {
        name: "cloudformation",
        title: "AWS CloudFormation",
        integration: "aws",
        place: &["region"],
        scope: None,
        settings: &[s("stack", true, "the stack"), s("template", true, "the template (SAM and CDK's synthesized output too)"), s("parameters", false, "the stack's parameters")],
        kinds: &[kind("cloudformation.aws.kiln.dev", "Stack", "Stack"), child(kind("cloudformation.aws.kiln.dev", "StackResource", "Resource"))],
        capabilities: &[Events, Drift, Adopt, Validate],
        reads: &["CloudFormation and SAM templates"],
        built: false,
    },
    ProviderDef {
        name: "gcp-run",
        title: "Cloud Run",
        integration: "gcp",
        place: &["project", "region"],
        scope: None,
        settings: &[s("service", true, "the service"), s("spec", false, "the service: a service.yaml in the source, or an object"), s("image", false, "the image (${{ artifacts.* }})")],
        kinds: &[kind("run.gcp.kiln.dev", "Service", "Service"), kind("run.gcp.kiln.dev", "Job", "Job"), child(kind("run.gcp.kiln.dev", "Revision", "Revision"))],
        capabilities: &[Traffic, Versions, Previews, Logs, Events, Drift, Adopt],
        reads: &["service.yaml"],
        built: false,
    },
    ProviderDef {
        name: "gcp-functions",
        title: "Cloud Run functions",
        integration: "gcp",
        place: &["project", "region"],
        scope: None,
        settings: &[s("function", true, "the function"), s("source", true, "the source or build (${{ artifacts.* }})")],
        kinds: &[kind("functions.gcp.kiln.dev", "Function", "Function")],
        capabilities: &[Traffic, Versions, Logs, Drift, Adopt],
        reads: &[],
        built: false,
    },
    ProviderDef {
        name: "gcp-appengine",
        title: "App Engine",
        integration: "gcp",
        place: &["project"],
        scope: None,
        settings: &[s("service", true, "the service"), s("version", false, "the version's name (by default the commit)")],
        kinds: &[kind("appengine.gcp.kiln.dev", "Service", "Service"), child(kind("appengine.gcp.kiln.dev", "Version", "Version"))],
        capabilities: &[Traffic, Versions, Logs, Drift, Adopt],
        reads: &["app.yaml"],
        built: false,
    },
    ProviderDef {
        name: "gcp-compute",
        title: "Compute Engine",
        integration: "gcp",
        place: &["project", "zone"],
        scope: None,
        settings: &[s("group", true, "the managed instance group"), s("template", true, "the instance template")],
        kinds: &[kind("compute.gcp.kiln.dev", "InstanceTemplate", "Instance template"), kind("compute.gcp.kiln.dev", "InstanceGroupManager", "Managed instance group"), child(kind("compute.gcp.kiln.dev", "Instance", "Instance"))],
        capabilities: &[Traffic, Events, Drift, Adopt],
        reads: &[],
        built: false,
    },
    ProviderDef {
        name: "azure-containerapps",
        title: "Azure Container Apps",
        integration: "azure",
        place: &["subscription", "resource-group"],
        scope: None,
        settings: &[s("app", true, "the container app"), s("spec", false, "the app's YAML in the source, or an object"), s("image", false, "the image (${{ artifacts.* }})")],
        kinds: &[kind("app.azure.kiln.dev", "ContainerApp", "Container app"), kind("app.azure.kiln.dev", "Job", "Job"), child(kind("app.azure.kiln.dev", "Revision", "Revision"))],
        capabilities: &[Traffic, Versions, Previews, Logs, Events, Drift, Adopt],
        reads: &["the app's YAML"],
        built: false,
    },
    ProviderDef {
        name: "azure-functions",
        title: "Azure Functions",
        integration: "azure",
        place: &["subscription", "resource-group"],
        scope: None,
        settings: &[s("app", true, "the function app"), s("package", true, "the package (${{ artifacts.* }})"), s("slot", false, "a deployment slot to swap through")],
        kinds: &[kind("functions.azure.kiln.dev", "FunctionApp", "Function app"), kind("functions.azure.kiln.dev", "Slot", "Slot")],
        capabilities: &[Traffic, Logs, Drift, Adopt],
        reads: &["host.json"],
        built: false,
    },
    ProviderDef {
        name: "azure-appservice",
        title: "Azure App Service",
        integration: "azure",
        place: &["subscription", "resource-group"],
        scope: None,
        settings: &[s("app", true, "the web app"), s("package", false, "a package (${{ artifacts.* }})"), s("image", false, "an image (${{ artifacts.* }})"), s("slot", false, "a deployment slot to swap through")],
        kinds: &[kind("appservice.azure.kiln.dev", "WebApp", "Web app"), kind("appservice.azure.kiln.dev", "Slot", "Slot")],
        capabilities: &[Traffic, Logs, Drift, Adopt],
        reads: &[],
        built: false,
    },
    ProviderDef {
        name: "azure-vmss",
        title: "Azure Virtual Machine Scale Sets",
        integration: "azure",
        place: &["subscription", "resource-group"],
        scope: None,
        settings: &[s("scale-set", true, "the scale set"), s("image", true, "the image version (${{ artifacts.* }})")],
        kinds: &[kind("compute.azure.kiln.dev", "ScaleSet", "Scale set"), child(kind("compute.azure.kiln.dev", "ScaleSetInstance", "Instance"))],
        capabilities: &[Events, Drift, Adopt],
        reads: &[],
        built: false,
    },
    ProviderDef {
        name: "azure-static",
        title: "Azure Static Web Apps",
        integration: "azure",
        place: &["subscription", "resource-group"],
        scope: None,
        settings: &[s("app", true, "the static web app"), s("site", true, "the built site (${{ artifacts.* }})")],
        kinds: &[kind("staticwebapps.azure.kiln.dev", "StaticSite", "Static web app")],
        capabilities: &[Previews, Drift, Adopt],
        reads: &["staticwebapp.config.json"],
        built: false,
    },
    ProviderDef {
        name: "cloudflare-workers",
        title: "Cloudflare Workers",
        integration: "cloudflare",
        place: &[],
        scope: None,
        settings: &[s("name", false, "the Worker (by default the wrangler config's name)"), s("bundle", true, "the bundle CI built (${{ artifacts.* }})"), s("config", false, "a wrangler.jsonc or wrangler.toml in the source (found by default)")],
        kinds: &[
            traffic(kind("workers.cloudflare.kiln.dev", "Worker", "Worker")),
            kind("workers.cloudflare.kiln.dev", "Route", "Route"),
            kind("workers.cloudflare.kiln.dev", "CustomDomain", "Custom domain"),
            child(kind("workers.cloudflare.kiln.dev", "Version", "Version")),
            child(kind("workers.cloudflare.kiln.dev", "Deployment", "Deployment")),
        ],
        capabilities: &[Traffic, Versions, Previews, Logs, Drift, Adopt],
        reads: &["wrangler.toml", "wrangler.jsonc"],
        built: true,
    },
    ProviderDef {
        name: "cloudflare-pages",
        title: "Cloudflare Pages",
        integration: "cloudflare",
        place: &[],
        scope: None,
        settings: &[s("project", true, "the Pages project"), s("site", true, "the built site (${{ artifacts.* }})")],
        kinds: &[kind("pages.cloudflare.kiln.dev", "Project", "Project"), child(kind("pages.cloudflare.kiln.dev", "Deployment", "Deployment"))],
        capabilities: &[Versions, Previews, Drift, Adopt],
        reads: &["wrangler.toml"],
        built: false,
    },
    ProviderDef {
        name: "vercel",
        title: "Vercel",
        integration: "vercel",
        place: &[],
        scope: None,
        settings: &[s("project", true, "the project"), s("prebuilt", false, "the prebuilt output CI made (${{ artifacts.* }}); otherwise Vercel builds the commit"), s("domains", false, "domains to keep on the project")],
        kinds: &[
            kind("vercel.kiln.dev", "Project", "Project"),
            traffic(kind("vercel.kiln.dev", "Production", "Production deployment")),
            scoped(kind("vercel.kiln.dev", "Domain", "Domain")),
            scoped(kind("vercel.kiln.dev", "EnvVar", "Environment variable")),
            child(kind("vercel.kiln.dev", "Deployment", "Deployment")),
        ],
        capabilities: &[Traffic, Versions, Previews, Logs, Drift, Adopt],
        reads: &["vercel.json"],
        built: true,
    },
    ProviderDef {
        name: "railway",
        title: "Railway",
        integration: "railway",
        place: &["project", "environment"],
        scope: None,
        settings: &[s("service", true, "the service"), s("image", false, "an image (${{ artifacts.* }}); otherwise the commit")],
        kinds: &[kind("railway.kiln.dev", "Service", "Service"), kind("railway.kiln.dev", "Variable", "Variable"), child(kind("railway.kiln.dev", "Deployment", "Deployment"))],
        capabilities: &[Versions, Logs, Drift, Adopt],
        reads: &["railway.json", "railway.toml"],
        built: false,
    },
    ProviderDef {
        name: "fly",
        title: "Fly.io",
        integration: "fly",
        place: &[],
        scope: None,
        settings: &[s("app", true, "the app"), s("image", true, "the image (${{ artifacts.* }})"), s("config", false, "a fly.toml in the source (found by default)")],
        kinds: &[kind("fly.kiln.dev", "App", "App"), kind("fly.kiln.dev", "Machine", "Machine"), stateful(kind("fly.kiln.dev", "Volume", "Volume")), kind("fly.kiln.dev", "Secret", "Secret")],
        capabilities: &[Traffic, Logs, Drift, Adopt],
        reads: &["fly.toml"],
        built: false,
    },
    ProviderDef {
        name: "netlify",
        title: "Netlify",
        integration: "netlify",
        place: &[],
        scope: None,
        settings: &[s("site", true, "the site"), s("dir", true, "the built site (${{ artifacts.* }})")],
        kinds: &[kind("netlify.kiln.dev", "Site", "Site"), child(kind("netlify.kiln.dev", "Deploy", "Deploy"))],
        capabilities: &[Versions, Previews, Drift, Adopt],
        reads: &["netlify.toml"],
        built: false,
    },
    ProviderDef {
        name: "render",
        title: "Render",
        integration: "render",
        place: &[],
        scope: None,
        settings: &[s("service", true, "the service"), s("image", false, "an image (${{ artifacts.* }}); otherwise the commit")],
        kinds: &[kind("render.kiln.dev", "Service", "Service"), child(kind("render.kiln.dev", "Deploy", "Deploy"))],
        capabilities: &[Versions, Logs, Drift, Adopt],
        reads: &["render.yaml"],
        built: false,
    },
    ProviderDef {
        name: "heroku",
        title: "Heroku",
        integration: "heroku",
        place: &[],
        scope: None,
        settings: &[s("app", true, "the app"), s("image", false, "an image (${{ artifacts.* }}); otherwise a build of the source")],
        kinds: &[kind("heroku.kiln.dev", "App", "App"), child(kind("heroku.kiln.dev", "Release", "Release")), child(kind("heroku.kiln.dev", "Build", "Build"))],
        capabilities: &[Versions, Logs, Drift, Adopt],
        reads: &["app.json", "heroku.yml"],
        built: false,
    },
    ProviderDef {
        name: "digitalocean",
        title: "DigitalOcean App Platform",
        integration: "digitalocean",
        place: &[],
        scope: None,
        settings: &[s("app", true, "the app"), s("spec", false, "the app spec: .do/app.yaml in the source (found by default), or an object")],
        kinds: &[kind("apps.digitalocean.kiln.dev", "App", "App"), child(kind("apps.digitalocean.kiln.dev", "Deployment", "Deployment"))],
        capabilities: &[Versions, Logs, Drift, Adopt],
        reads: &[".do/app.yaml"],
        built: false,
    },
    ProviderDef {
        name: "nomad",
        title: "Nomad",
        integration: "nomad",
        place: &[],
        scope: None,
        settings: &[s("job", true, "the job: HCL or JSON in the source")],
        kinds: &[scoped(kind("nomad.kiln.dev", "Job", "Job")), child(scoped(kind("nomad.kiln.dev", "Deployment", "Deployment"))), child(scoped(kind("nomad.kiln.dev", "Allocation", "Allocation")))],
        capabilities: &[Traffic, Logs, Events, Drift, Adopt, Validate],
        reads: &["job HCL or JSON"],
        built: false,
    },
    ProviderDef {
        name: "host",
        title: "Hosts",
        integration: "runner",
        place: &["selector"],
        scope: None,
        settings: &[s("service", true, "the service (systemd, launchd or a Windows service)"), s("files", true, "what it installs (${{ artifacts.* }})"), s("check", false, "a check each host runs once it restarts")],
        kinds: &[kind("host.kiln.dev", "Service", "Service"), child(kind("host.kiln.dev", "Host", "Host"))],
        capabilities: &[Logs, Drift],
        reads: &[],
        built: false,
    },
    ProviderDef {
        name: "compose",
        title: "Docker Compose",
        integration: "runner",
        place: &["selector"],
        scope: None,
        settings: &[s("project", true, "the Compose project"), s("file", false, "compose.yaml in the source (found by default)")],
        kinds: &[kind("compose.kiln.dev", "Project", "Project"), child(kind("compose.kiln.dev", "Container", "Container"))],
        capabilities: &[Logs, Drift],
        reads: &["compose.yaml"],
        built: false,
    },
    ProviderDef {
        name: "microvm",
        title: "microVMs",
        integration: "runner",
        place: &["selector"],
        scope: None,
        settings: &[s("vm", true, "the microVM"), s("image", true, "its image or Agentfile (${{ artifacts.* }})")],
        kinds: &[kind("microvm.kiln.dev", "VM", "microVM")],
        capabilities: &[Logs, Drift],
        reads: &["Agentfile"],
        built: false,
    },
    ProviderDef {
        name: "terraform",
        title: "Terraform and OpenTofu",
        integration: "runner",
        place: &[],
        scope: None,
        settings: &[s("workspace", true, "the workspace"), s("dir", false, "the configuration's directory in the source"), s("binary", false, "terraform or tofu"), s("vars", false, "input variables")],
        kinds: &[kind("terraform.kiln.dev", "Workspace", "Workspace"), child(kind("terraform.kiln.dev", "Resource", "Resource"))],
        capabilities: &[Drift, Validate],
        reads: &["*.tf"],
        built: false,
    },
    ProviderDef {
        name: "pulumi",
        title: "Pulumi",
        integration: "runner",
        place: &[],
        scope: None,
        settings: &[s("stack", true, "the stack"), s("dir", false, "the project's directory in the source")],
        kinds: &[kind("pulumi.kiln.dev", "Stack", "Stack"), child(kind("pulumi.kiln.dev", "Resource", "Resource"))],
        capabilities: &[Drift, Validate],
        reads: &["Pulumi.yaml"],
        built: false,
    },
    ProviderDef {
        name: "job",
        title: "A workflow job",
        integration: "runner",
        place: &[],
        scope: None,
        settings: &[s("workflow", true, "the workflow"), s("job", false, "the job in it"), s("check", false, "a check after it")],
        kinds: &[kind("job.kiln.dev", "Run", "Run")],
        capabilities: &[Logs],
        reads: &["a workflow"],
        built: false,
    },
];

/// The provider named `name`.
pub fn get(name: &str) -> Option<&'static ProviderDef> {
    CATALOG.iter().find(|p| p.name == name)
}

/// The provider whose kinds are in `group` (`ecs.aws.kiln.dev`).
pub fn for_group(group: &str) -> Option<&'static ProviderDef> {
    if !group.ends_with("kiln.dev") {
        return None;
    }
    CATALOG.iter().find(|p| p.kinds.iter().any(|k| k.group == group))
}

/// The kind `kind` of `group`.
pub fn kind_def(group: &str, kind: &str) -> Option<&'static KindDef> {
    for_group(group)?.kinds.iter().find(|k| k.group == group && k.kind == kind)
}

// ------------------------------------------------------------------ targets

/// A target (§1.1): a provider, the account it acts through, and a place. Its name is the
/// engine's cluster name for it: `aws-ecs/aws-prod/us-east-1`, or `vercel/vercel` for a provider
/// placed by its account alone.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Target {
    pub provider: String,
    pub account: String,
    /// The place's values, in the provider's order.
    pub place: Vec<String>,
}

impl Target {
    /// Its name: the engine's cluster.
    pub fn name(&self) -> String {
        let mut parts = vec![self.provider.as_str(), self.account.as_str()];
        parts.extend(self.place.iter().map(String::as_str));
        parts.join("/")
    }

    /// The target a cluster name stands for, if it names one: its first part a provider, then an
    /// account, then as many place parts as the provider takes.
    pub fn parse(name: &str) -> Option<Target> {
        let mut parts = name.split('/');
        let def = get(parts.next()?)?;
        let account = parts.next().filter(|a| !a.is_empty())?.to_owned();
        let place: Vec<String> = parts.map(str::to_owned).collect();
        (place.len() == def.place.len() && place.iter().all(|p| !p.is_empty())).then(|| Target { provider: def.name.to_owned(), account, place })
    }

    /// Its provider.
    pub fn def(&self) -> Option<&'static ProviderDef> {
        get(&self.provider)
    }

    /// A place value by name (`region`).
    pub fn place_value(&self, key: &str) -> Option<&str> {
        let i = self.def()?.place.iter().position(|k| *k == key)?;
        self.place.get(i).map(String::as_str)
    }
}

/// Whether a cluster name is a target's.
pub fn is_target(cluster: &str) -> bool {
    Target::parse(cluster).is_some()
}

/// An environment's target for a provider, from `.kiln/environments.yaml`'s
/// `targets: { aws-ecs: { account: aws-prod, region: us-east-1 } }`: the target, and its other
/// settings (`worker-suffix`), which reach the provider's render.
pub fn target_of(provider: &str, spec: &Map<String, Value>) -> Result<(Target, Map<String, Value>), String> {
    let def = get(provider).ok_or_else(|| format!("no provider {provider} (there are {})", CATALOG.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")))?;
    let text = |k: &str| spec.get(k).and_then(|v| v.as_str().map(str::to_owned).or_else(|| v.as_i64().map(|n| n.to_string()))).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let account = text("account").ok_or_else(|| format!("the {provider} target needs an account: the integration it acts through"))?;
    let mut place = Vec::new();
    for k in def.place {
        let v = text(k).ok_or_else(|| format!("the {provider} target needs {k}"))?;
        if v.contains('/') {
            return Err(format!("{k} `{v}` can't hold `/`"));
        }
        place.push(v);
    }
    if account.contains('/') {
        return Err(format!("account `{account}` can't hold `/`"));
    }
    let extra: Map<String, Value> = spec.iter().filter(|(k, _)| *k != "account" && !def.place.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    Ok((Target { provider: def.name.to_owned(), account, place }, extra))
}

// ------------------------------------------------------------------ rendering

/// What a provider's render reads.
pub struct RenderInput<'a> {
    /// The app's definition: its provider key, interpolated.
    pub spec: &'a Value,
    /// The target's settings beyond its place (`worker-suffix`).
    pub target: &'a Map<String, Value>,
    /// The app.
    pub app: &'a str,
    /// The destination's namespace: the provider's scope, when its kinds have one.
    pub namespace: &'a str,
    /// The commit.
    pub revision: &'a str,
    /// A file of the source, by its path relative to the source's directory.
    pub read: &'a dyn Fn(&str) -> Option<Vec<u8>>,
}

/// Renders an app's definition for `provider` into objects.
pub fn render(provider: &str, input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    match provider {
        "aws-ecs" => aws_ecs::render(input),
        "aws-lambda" => aws_lambda::render(input),
        "cloudflare-workers" => cloudflare_workers::render(input),
        "vercel" => vercel::render(input),
        other => match get(other) {
            Some(p) => Err(format!("{} isn't built yet: kiln lists it, and deploys to it once it is", p.title)),
            None => Err(format!("no provider {other}")),
        },
    }
}

/// The files of the source a provider's render reads for `spec`, relative to the source's
/// directory: what the render key covers.
pub fn files(provider: &str, spec: &Value) -> Vec<String> {
    match provider {
        "aws-ecs" => aws_ecs::files(spec),
        "aws-lambda" => aws_lambda::files(spec),
        "cloudflare-workers" => cloudflare_workers::files(spec),
        "vercel" => vercel::files(spec),
        _ => Vec::new(),
    }
}

/// The health of a provider's object, by its group (§2.5); `None` for kinds with no rule.
pub fn health(obj: &Value) -> Option<Status> {
    let group = crate::obj::group(obj);
    match for_group(group)?.name {
        "aws-ecs" => aws_ecs::health(obj),
        "aws-lambda" => aws_lambda::health(obj),
        "cloudflare-workers" => cloudflare_workers::health(obj),
        "vercel" => vercel::health(obj),
        _ => None,
    }
}

// ------------------------------------------------------------------ helpers

/// An object of a provider's kind: `apiVersion`, `kind`, `metadata` (name, and namespace when
/// given), `spec`.
pub fn object(group: &str, kind: &str, name: &str, namespace: Option<&str>, spec: Value) -> Value {
    let mut meta = Map::new();
    meta.insert("name".into(), Value::String(name.to_owned()));
    if let Some(ns) = namespace.filter(|n| !n.is_empty()) {
        meta.insert("namespace".into(), Value::String(ns.to_owned()));
    }
    let mut o = Map::new();
    o.insert("apiVersion".into(), Value::String(format!("{group}/v1")));
    o.insert("kind".into(), Value::String(kind.to_owned()));
    o.insert("metadata".into(), Value::Object(meta));
    o.insert("spec".into(), spec);
    Value::Object(o)
}

/// A setting that is a JSON (or YAML) file of the source, or the object itself.
pub fn file_or_object(input: &RenderInput<'_>, key: &str) -> Result<Option<Value>, String> {
    match input.spec.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path)) => {
            let bytes = (input.read)(path).ok_or_else(|| format!("{key}: no file {path} in the source"))?;
            let text = String::from_utf8_lossy(&bytes);
            let v: Value = if path.ends_with(".json") { serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))? } else { serde_yaml::from_str(&text).map_err(|e| format!("{path}: {e}"))? };
            Ok(Some(v))
        }
        Some(other) => Ok(Some(other.clone())),
    }
}

/// The settings' paths that are files of the source, for [`files`].
pub fn file_settings(spec: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter().filter_map(|k| spec.get(*k).and_then(Value::as_str)).filter(|p| !p.contains("${{")).map(str::to_owned).collect()
}

/// A setting's text, trimmed; empty is none.
pub fn text<'a>(spec: &'a Value, key: &str) -> Option<&'a str> {
    spec.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Sets a value by path, as `set:` writes them: `containerDefinitions[name=api].image`,
/// `Environment.Variables.MODE`, `containerDefinitions[0].cpu`. Objects on the way are made;
/// list items are found by `[field=value]` or by index.
pub fn set_path(target: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let mut cur = target;
    let segs = path_segments(path)?;
    let last = segs.len().saturating_sub(1);
    for (i, seg) in segs.iter().enumerate() {
        let at_end = i == last;
        match seg {
            Seg::Field(name) => {
                if !cur.is_object() {
                    *cur = Value::Object(Map::new());
                }
                let Value::Object(m) = cur else { return Err(format!("{path}: not an object")) };
                if at_end {
                    m.insert(name.clone(), value);
                    return Ok(());
                }
                cur = m.entry(name.clone()).or_insert(Value::Null);
            }
            Seg::Index(n) => {
                let Value::Array(a) = cur else { return Err(format!("{path}: [{n}] of something that isn't a list")) };
                let item = a.get_mut(*n).ok_or_else(|| format!("{path}: no item {n}"))?;
                if at_end {
                    *item = value;
                    return Ok(());
                }
                cur = item;
            }
            Seg::Match(field, want) => {
                let Value::Array(a) = cur else { return Err(format!("{path}: [{field}={want}] of something that isn't a list")) };
                let item = a.iter_mut().find(|v| v.get(field).is_some_and(|f| f.as_str().map(str::to_owned).unwrap_or_else(|| f.to_string()) == *want)).ok_or_else(|| format!("{path}: no item with {field}={want}"))?;
                if at_end {
                    *item = value;
                    return Ok(());
                }
                cur = item;
            }
        }
    }
    Err(format!("{path}: an empty path"))
}

#[derive(Debug, PartialEq, Eq)]
enum Seg {
    Field(String),
    Index(usize),
    Match(String, String),
}

fn path_segments(path: &str) -> Result<Vec<Seg>, String> {
    let mut out = Vec::new();
    let mut rest = path.trim();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('[') {
            let (inner, after) = r.split_once(']').ok_or_else(|| format!("{path}: a `[` without `]`"))?;
            match inner.split_once('=') {
                Some((f, v)) => out.push(Seg::Match(f.trim().to_owned(), v.trim().trim_matches('"').to_owned())),
                None => out.push(Seg::Index(inner.trim().parse().map_err(|_| format!("{path}: `[{inner}]` is neither an index nor field=value"))?)),
            }
            rest = after.strip_prefix('.').unwrap_or(after);
            continue;
        }
        let end = rest.find(['.', '[']).unwrap_or(rest.len());
        let (name, after) = rest.split_at(end);
        if name.is_empty() {
            return Err(format!("{path}: an empty name"));
        }
        out.push(Seg::Field(name.to_owned()));
        rest = after.strip_prefix('.').unwrap_or(after);
    }
    if out.is_empty() {
        return Err("an empty path".into());
    }
    Ok(out)
}

/// Applies an app's `set:` map to `target`.
pub fn apply_sets(target: &mut Value, sets: Option<&Value>) -> Result<(), String> {
    let Some(Value::Object(m)) = sets else { return Ok(()) };
    for (path, v) in m {
        set_path(target, path, v.clone())?;
    }
    Ok(())
}

/// Health from a provider rule.
pub fn status(health: crate::model::Health, message: impl Into<String>) -> Option<Status> {
    Some(Status { health, message: message.into() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn targets_name_and_parse() {
        let t = Target { provider: "aws-ecs".into(), account: "aws-prod".into(), place: vec!["us-east-1".into()] };
        assert_eq!(t.name(), "aws-ecs/aws-prod/us-east-1");
        assert_eq!(Target::parse("aws-ecs/aws-prod/us-east-1"), Some(t.clone()));
        assert_eq!(t.place_value("region"), Some("us-east-1"));
        // A cluster's name isn't a target's; a target needs its whole place.
        assert_eq!(Target::parse("in-cluster"), None);
        assert_eq!(Target::parse("aws-ecs/aws-prod"), None);
        assert_eq!(Target::parse("gcp-run/gcp/proj"), None);
        assert!(Target::parse("gcp-run/gcp/proj/us-central1").is_some());
        assert_eq!(Target::parse("vercel/vercel").map(|t| t.name()), Some("vercel/vercel".to_owned()));
        let spec = json!({ "account": "aws-prod", "region": "us-east-1", "worker-suffix": "-staging" });
        let (t2, extra) = target_of("aws-ecs", spec.as_object().unwrap_or(&Map::new())).unwrap_or_else(|e| (Target { provider: e, account: String::new(), place: Vec::new() }, Map::new()));
        assert_eq!(t2, t);
        assert_eq!(extra.get("worker-suffix"), Some(&json!("-staging")));
        assert!(target_of("aws-ecs", json!({ "region": "us-east-1" }).as_object().unwrap_or(&Map::new())).is_err());
        assert!(target_of("nope", &Map::new()).is_err());
    }

    #[test]
    fn every_provider_is_named_once_and_owns_its_groups() {
        let mut names: Vec<&str> = CATALOG.iter().map(|p| p.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CATALOG.len());
        for p in CATALOG {
            assert!(!p.kinds.is_empty(), "{} has no kinds", p.name);
            for k in p.kinds {
                assert!(k.group.ends_with("kiln.dev"), "{}", k.group);
                assert_eq!(for_group(k.group).map(|q| q.name), Some(p.name), "{} is another provider's group too", k.group);
            }
        }
    }

    #[test]
    fn paths_set_values() {
        let mut v = json!({ "containerDefinitions": [{ "name": "api", "image": "old" }, { "name": "side", "image": "x" }] });
        assert_eq!(set_path(&mut v, "containerDefinitions[name=api].image", json!("new")), Ok(()));
        assert_eq!(v.pointer("/containerDefinitions/0/image"), Some(&json!("new")));
        assert_eq!(set_path(&mut v, "containerDefinitions[1].cpu", json!(256)), Ok(()));
        assert_eq!(v.pointer("/containerDefinitions/1/cpu"), Some(&json!(256)));
        assert_eq!(set_path(&mut v, "Environment.Variables.MODE", json!("prod")), Ok(()));
        assert_eq!(v.pointer("/Environment/Variables/MODE"), Some(&json!("prod")));
        assert!(set_path(&mut v, "containerDefinitions[name=nope].image", json!(1)).is_err());
        assert!(set_path(&mut v, "a[", json!(1)).is_err());
    }
}
