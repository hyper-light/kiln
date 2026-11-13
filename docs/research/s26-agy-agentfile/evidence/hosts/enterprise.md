# Antigravity in Gemini Enterprise

To deploy Google Antigravity using models hosted directly within your organization’s Google Cloud infrastructure, you can integrate with Gemini Enterprise. Every session runs under Google Cloud’s enterprise security controls, data residency guarantees, and the Google Cloud Terms of Service.

Supported products: [Antigravity App](/product/antigravity-2) [Antigravity CLI](/product/antigravity-cli) [Visual Studio Code](/docs/ide/extensions/vscode) [Visual Studio](/docs/ide/extensions/visual-studio) [JetBrains (Preview)](/docs/ide/extensions/jetbrains) [Zed (Preview)](/docs/ide/extensions/zed) [Xcode (Preview)](/docs/ide/extensions/xcode) [Eclipse (Preview)](/docs/ide/extensions/eclipse)

Note

Enterprise integration is supported for the Antigravity App, Antigravity CLI, and Antigravity IDE extensions. **Antigravity IDE** (standalone) is not supported for enterprise deployments, and legacy **Gemini Code Assist** IDE extensions support only first-party Gemini models.

[Developer sign-in and models](#sign-in-and-license-selection)Sign in with Google Cloud SSO, Workforce Identity Federation (WIF), or ADC and select enterprise models.

[Administrator and network setup](#administrator-setup-guide)Provision a Google Cloud project, enable third-party models, and configure firewall and mTLS allowlists.

## Overview and key benefits

You can connect Antigravity to Gemini Enterprise in two ways:

*   **Google Cloud project and API**: Connect directly using a Google Cloud project with an invoiced Cloud Billing account and the `roles/discoveryengine.agentspaceUser` Identity and Access Management (IAM) role for consumption-based billing.
*   **Gemini Enterprise license**: Connect with your **Gemini Enterprise Standard** or **Gemini Enterprise Plus** seat license to get access to included first-party quotas, managed overages, and centralized administrative controls across the Antigravity App, Antigravity CLI (`agy`), and Antigravity IDE extensions. No separate per-user Antigravity license is required.

By connecting Google Antigravity to your Google Cloud project, your organization gains:

Enterprise governance

Operates under your existing Google Cloud Terms of Service with centralized administrative controls.

Data residency and security

Satisfies private networking (VPC Service Controls). Regional data residency constraints apply to first-party Gemini models; third-party model inference runs in `global`, `us`, and `eu` locations (in-country regions are not supported; see Anthropic’s [Supported countries and regions](https://www.anthropic.com/supported-countries)).

## Sign in and license selection

After installing the [Antigravity App](/docs/getting-started), the [Antigravity CLI (`agy`)](/docs/cli/install), or a supported [IDE extension](/docs/ide/extensions), choose your organization’s authentication method to connect to Gemini Enterprise:

### Google Cloud SSO

#### Sign-in workflow

Google Antigravity uses a single sign-on (SSO) flow that automatically detects your assigned license tier:

1.  Launch the **Antigravity App**, run `agy` (or run `/logout` inside `agy` to switch accounts) in the **Antigravity CLI**, or open your **[IDE extension](/docs/ide/extensions)**.
2.  Select **Sign in** to open the browser authentication flow.
3.  Choose **Business account** _(subject to the Google Cloud Terms of Service)_.
4.  Select **Continue with Google Cloud** and complete authentication in your browser.
5.  In the **License Selector**, confirm the Google Cloud project linked to your license and select it. Alternatively, select **Other** (**Other License** in the App, or **Can’t find your subscription? Enter a Google Cloud project** in the CLI) to self-assign a license by entering your project ID and selecting a location (`global`, `us`, or `eu`). In the CLI, your project and location are saved under [`gcp`](/docs/settings?tab=cli#custommodelsconfig-and-gcp) in `~/.gemini/antigravity-cli/settings.json` to pre-fill future sign-ins.

Note

**Data-sharing and project logging notice**: Your customer telemetry and model interactions are logged directly to the Google Cloud project corresponding to the license you select. You can maintain **one license per project and location**.

### BYOID and WIF

#### Configuring Bring Your Own Identity (BYOID)

Bring Your Own Identity (BYOID) uses Workforce Identity Federation (WIF) to let your organization authenticate through an external identity provider, such as Okta, instead of a standard Google Account:

1.  In Antigravity, select **Business account**.
2.  Select **Advanced WIF Configuration**.
3.  Enter the **WIF Configuration String** (for example, `locations/global/workforcePools/POOL_ID/providers/PROVIDER_ID`) provided by your organization’s administrator.
4.  Complete sign-in through your federated identity provider.
5.  Select or self-assign a license from the **License Selector**.

Note

If the same email address exists across multiple identity providers, sign in with the identity that matches your Gemini Enterprise license.

### ADC (Headless)

#### Setting up Application Default Credentials (ADC)

For headless environments and automated terminal workflows, Antigravity supports authentication using Google Cloud [Application Default Credentials](https://docs.cloud.google.com/docs/authentication/application-default-credentials) (ADC) across the **Antigravity CLI**, **Antigravity App**, and **Antigravity IDE**.

1.  Generate local Application Default Credentials for your project using the Google Cloud SDK:
    
    ```bash
    gcloud auth application-default login --project {GCP_PROJECT}
    ```
    
2.  Verify that your credentials file exists (on Linux and macOS, `~/.config/gcloud/application_default_credentials.json`).
    
3.  Enable ADC for your surface:
    
    *   **Antigravity CLI**: Export `AGY_ADC_AUTH=true` before running `agy` (run `unset AGY_ADC_AUTH` to sign out):
        
        ```bash
        export AGY_ADC_AUTH=true
        ```
        
    *   **Antigravity App and IDE**: Launch from a terminal with `AGY_ADC_AUTH=true`, or set `"enableAdc": true` in `~/.gemini/config/config.json`:
        
        ```json
        {
          "userSettings": {
            "enableAdc": true
          }
        }
        ```
        

#### How ADC resolves credentials, project, and location

*   **Credentials**: Resolved using the [standard ADC search order](https://docs.cloud.google.com/docs/authentication/application-default-credentials#order).
*   **Project ID**: Resolved in order from (1) `quota_project_id` in the ADC file, (2) `GOOGLE_CLOUD_QUOTA_PROJECT` environment variable _(CLI only)_, or (3) the metadata server project ID (for service accounts).
*   **Location**: Resolved from `GOOGLE_CLOUD_LOCATION` _(CLI)_ or the `location` field in the ADC file _(Antigravity App / IDE)_, defaulting to `global`.

Note

When authenticating with ADC, models older than Gemini 3 Flash are not supported, and image generation is not available in `eu` and `us` locations.

## Supported models in Gemini Enterprise

Antigravity in Gemini Enterprise supports included first-party Gemini models and administrator-enabled third-party partner models:

| Model | CLI / API Identifier | Enterprise |
| --- | --- | --- |
| [Gemini 4 Carbon](/blog/gemini-4-carbon-in-google-antigravity) | `gemini-4-carbon` | ✅ |
| [Gemini 4 Argon](/blog/gemini-4-argon-in-google-antigravity) | `gemini-4-argon` | ✅ |
| [Gemini 4 Hydrogen](/blog/gemini-4-hydrogen-in-google-antigravity) | `gemini-4-hydrogen` | ✅ |
| [Orchestrator (Carbon)](/docs/models#orchestrator) | — | ❌ |
| [Orchestrator (Argon)](/docs/models#orchestrator) | — | ❌ |
| [Gemini 3.8 Flash](/blog/gemini-3-8-flash-in-google-antigravity) | `gemini-3.8-flash` | ✅ |
| [Gemini 3.7 Flash](/blog/gemini-3-7-flash-in-google-antigravity) | `gemini-3.7-flash` | ✅ |
| [Gemini 3.6 Flash](/blog/gemini-3-6-flash-in-google-antigravity) | `gemini-3.6-flash` | ✅ |
| [Gemini 3.1 Pro](/blog/gemini-3-1-pro-in-google-antigravity) | `gemini-3.1-pro-preview` | ✅ |
| [Claude Sonnet 5.5 (thinking)](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/partner-models/claude/sonnet-5-5) | `claude-sonnet-5-5-<level>`\* | ✅\*\* |
| [Claude Opus 5.5 (thinking)](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/partner-models/claude/opus-5-5) | `claude-opus-5-5-<level>`\* | ✅\*\* |
| Claude Sonnet 4.6 (thinking) | — | ❌ |
| Claude Opus 4.6 (thinking) | — | ❌ |
| GPT-OSS-120b | — | ❌ |

\* Available at four thinking levels (<level>): low, medium (default), high, and max (for example, claude-opus-5-5-high or claude-sonnet-5-5-max).

\*\* Third-party partner models require [administrator opt-in](https://cloud.google.com/gemini/enterprise/docs/ai-developer-tools-settings#enable-third-party-models) in the Google Cloud Console and are billed by token consumption. First-party Gemini models are enabled by default and draw from pooled edition seat credits ($10/month Standard, $15/month Plus).

### Selecting models and thinking levels

Once enabled for your project, models appear in your client shortly after your administrator enables them:

*   **Antigravity App and IDE extensions**: Open the **Model picker** in the chat composer and select your preferred model (third-party models appear under **Other Models**). **Anthropic Claude Opus 5.5** and **Anthropic Claude Sonnet 5.5** are available at four thinking levels—`Low`, `Medium` (default), `High`, and `Max`—and each level appears as a separate entry in the model picker (for example, **Claude Opus 5.5 (High)**). Higher levels give the model more room to reason on complex tasks, but use more tokens.
*   **Antigravity CLI (`agy`)**: Start a session with `--model` or switch mid-session with the `/model` slash command:
    
    ```bash
    agy --model claude-opus-5-5-medium
    agy --model claude-sonnet-5-5-medium
    ```
    
    You can also specify a base model ID and use the `--effort` flag (or `/effort` mid-session, requires `agy` `v1.2.11` or later) to set the reasoning level (`low`, `medium`, `high`, `xhigh`, or `max`, depending on the model):
    
    ```bash
    agy --model claude-opus-5-5 --effort high
    agy --model claude-sonnet-5-5 --effort high
    ```
    

### Architectural transparency: Hybrid execution

When you select **Anthropic Claude Opus 5.5** or **Anthropic Claude Sonnet 5.5** in Antigravity, your selected third-party model executes all primary reasoning, planning, tool orchestration, and code generation turns.

Antigravity uses a hybrid execution architecture: lightweight first-party Gemini models continue to run in the background to handle high-frequency client-side coordination—such as context management and summaries. These background models consume first-party tokens, which can incur charges.

## Autonomous tool permissions and human-in-the-loop controls

Autonomous tool execution in Antigravity supports **human-in-the-loop** confirmation when using either first-party Gemini models or administrator-enabled third-party Anthropic models (`claude-opus-5-5` and `claude-sonnet-5-5`):

*   **Antigravity App**: Keep **Default** (isolated terminal sandbox) or **Request Review** enabled in **Settings > General > Permission Settings** so unsandboxed shell commands, external file modifications, and MCP tool calls require explicit developer approval. Use unrestricted auto-approval (**Turbo** / **Always proceed**) only in isolated development containers or disposable branches.
*   **Antigravity CLI (`agy`)**: Keep interactive tool permissions and `--sandbox=true` enabled (`seatbelt` on macOS, `bubblewrap` on Linux), or use read-only plan mode (`--mode plan`) before approving edits. Configure policies with `/permissions` or `/config`.

[Agent permissions](/docs/permissions)Configure allow, ask, and deny rules and permission presets in the Antigravity App and the CLI.

[Terminal sandbox](/docs/sandbox)Learn how OS-level sandboxing isolates shell commands and network access.

## Administrator setup guide

### Gemini Enterprise subscription setup

To set up Gemini Enterprise subscriptions, assign user licenses, and configure IAM permissions (`roles/discoveryengine.agentspaceAdmin` for administrators and `roles/discoveryengine.agentspaceUser` for anyone using Antigravity, including administrators), follow the official [Google Cloud administrator setup guide](https://docs.cloud.google.com/gemini/enterprise/docs/before-you-begin). For an overview of supported developer capabilities, see the [AI developer tools overview](https://docs.cloud.google.com/gemini/enterprise/docs/ai-developer-tools-overview).

Enable AI developer tools for your region

In Gemini Enterprise, **AI developer tools may be disabled by default** (such as on existing subscriptions or additional regions) and must be enabled for the specific region (`global`, `us`, or `eu`) where your subscription is provisioned. An administrator must go to **Gemini Enterprise > Settings > AI developer tools** in the Google Cloud Console, select the target region, and turn on the **AI developer tools** toggle (which also enables the required `discoveryengine.googleapis.com` and `businessaicode.googleapis.com` APIs). For step-by-step instructions, see [Turn on AI developer tools](https://docs.cloud.google.com/gemini/enterprise/docs/ai-developer-tools-settings#enable-ai-developer-tools).

[Gemini Enterprise Setup Guide](https://docs.cloud.google.com/gemini/enterprise/docs/before-you-begin)

### Google Cloud project and API setup

New to Gemini Enterprise Agent Platform?

If you are a new customer connecting via API, you will need an API key to authenticate. Visit the [API Key Quickstart Guide](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/start/api-keys?usertype=expressmode) to create your key first.

Complete the following steps to provision your Google Cloud project and enable API access:

1.  **Select or create a Google Cloud project**: Select an existing project or create a dedicated project for your team’s Antigravity workloads.
    
    Note
    
    **Project switching note**: To switch to a different Google Cloud project or location, log out of the Antigravity CLI or the Antigravity App, then log back in to select your new project or region.
    
    [Go to GCP Project Selector](https://console.cloud.google.com/projectselector2)
2.  **Verify Cloud Billing**: Ensure that Cloud Billing is active for your selected Google Cloud project. You can inspect your project’s billing status in the Google Cloud Console.
    
    [Open Google Cloud Billing Console](https://console.cloud.google.com/billing)
3.  **Enable the API**: Enable the Gemini Enterprise API (`aiplatform.googleapis.com`) to allow Antigravity clients to connect to your project’s model endpoints.
    
    [Enable API in Cloud Console](https://console.cloud.google.com/apis/library/aiplatform.googleapis.com)
4.  **Enable third-party partner models (optional)**: To allow developers to use **Anthropic Claude Opus 5.5** or **Anthropic Claude Sonnet 5.5**, [enable third-party models](https://cloud.google.com/gemini/enterprise/docs/ai-developer-tools-settings#enable-third-party-models) in **Gemini Enterprise > Settings > AI developer tools** in the Google Cloud Console.
    

## Regional endpoints and network allowlists

### Regional endpoints

Antigravity CLI, the Antigravity App, and IDE extensions support multi-region deployment endpoints to satisfy regional data residency requirements:

| Region | Location ID | Supported capabilities |
| :-- | :-- | :-- |
| **Global** | `global` | Text generation, code inference, multimodal, image generation |
| **US** (Multi-region) | `us` | Text generation, code inference, multimodal |
| **EU** (Multi-region) | `eu` | Text generation, code inference, multimodal |

Note

Image generation capabilities are currently available exclusively on **`global`** deployment endpoints.

For full endpoint specifications, consult the [Deployment endpoints documentation](https://docs.cloud.google.com/gemini-enterprise-agent-platform/resources/locations#global).

### Network allowlists and mTLS endpoints

If your organization restricts outbound developer traffic using corporate firewalls, proxies, or context-aware access rules, allowlist the following endpoints over TCP port `443` for the Antigravity App, Antigravity CLI (`agy`), and Antigravity for IDEs:

| Service and protocol | Allowed hostnames (TCP port 443) |
| :-- | :-- |
| **AI Developer Tools Orchestration — Global, US, EU** (Standard HTTPS) | `businessaicode.googleapis.com`  
`businessaicode.us.rep.googleapis.com`  
`businessaicode.eu.rep.googleapis.com` |
| **AI Developer Tools Orchestration** (Mutual TLS) | `businessaicode.mtls.googleapis.com` |
| **Vertex AI / Gemini Enterprise API — Global, US, EU** (Standard HTTPS) | `aiplatform.googleapis.com`  
`aiplatform.us.rep.googleapis.com`  
`aiplatform.eu.rep.googleapis.com` |
| **Vertex AI / Gemini Enterprise API** (Mutual TLS) | `aiplatform.mtls.googleapis.com` |
| **Discovery Engine — Global, US, EU** (Standard HTTPS) | `discoveryengine.googleapis.com`  
`us-discoveryengine.googleapis.com`  
`eu-discoveryengine.googleapis.com` |
| **Discovery Engine — Global, US, EU** (Mutual TLS) | `discoveryengine.mtls.googleapis.com`  
`us-discoveryengine.mtls.googleapis.com`  
`eu-discoveryengine.mtls.googleapis.com` |
| **Cloud Code Telemetry & Client Services** (Standard HTTPS) | `cloudcode-pa.googleapis.com` |

Note

**Global endpoints required across all regions**: Even when using the `us` or `eu` multi-region location, the client connects to the global endpoints (`businessaicode.googleapis.com` and `cloudcode-pa.googleapis.com`) during sign-in to discover assigned licenses and load the model catalog before routing inference traffic to `*.<location>.rep.googleapis.com`. Ensure both global endpoints remain reachable for all regions.

### Private Service Connect (PSC) for regional endpoints

When you select the `us` or `eu` location, Antigravity connects to multi-regional endpoints (`*.<location>.rep.googleapis.com`), such as `businessaicode.us.rep.googleapis.com` and `aiplatform.us.rep.googleapis.com`.

Global Private Google Access VIPs (`private.googleapis.com` and `restricted.googleapis.com`) and global Private Service Connect (PSC) endpoints serve a single-level wildcard TLS certificate for `*.googleapis.com` that does not match multi-level `*.<location>.rep.googleapis.com` subdomains. If your network routes Google APIs privately through PSC, Private Google Access, or VPC Service Controls in the `us` or `eu` locations, complete the following steps:

1.  **Create a PSC regional endpoint**: Configure a [Private Service Connect regional endpoint](https://docs.cloud.google.com/vpc/docs/access-regional-google-apis-endpoints) in your VPC for each multi-regional API you use in your selected region (`businessaicode.<location>.rep.googleapis.com`, and `aiplatform.<location>.rep.googleapis.com` when using pay-as-you-go billing or Application Default Credentials).
2.  **Configure a private Cloud DNS zone**: Create a private Cloud DNS zone for each regional hostname (for example, `businessaicode.us.rep.googleapis.com.`) with an `A` record pointing to the reserved internal regional PSC IP address.
3.  **Keep global endpoints routed through your global endpoint**: Route `businessaicode.googleapis.com` and `cloudcode-pa.googleapis.com` through your [global PSC endpoint](https://docs.cloud.google.com/vpc/docs/configure-private-service-connect-apis) or Private Google Access configuration so initial license discovery and model catalog requests succeed.

## Security and governance

Request and response logging

Audit model interactions and maintain enterprise compliance records for your Gemini Enterprise instance. [Learn more](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/capabilities/request-response-logging)

VPC Service Controls (VPC-SC)

Enforce private networking security perimeters by adding the Business AI Code API (`businessaicode.googleapis.com`) and Gemini Enterprise API (`aiplatform.googleapis.com`) to your VPC-SC perimeter. [Learn more](https://docs.cloud.google.com/gemini-enterprise-agent-platform/machine-learning/general/vpc-service-controls)

## Troubleshooting and diagnostics

### Common sign-in and license issues

Review the following solutions for common sign-in and license issues:

*   **No licenses appear during setup**: Licenses are assigned by your organization’s Google Cloud administrator. If the License Selector is empty, contact your administrator to ensure your account has been granted access to a Gemini Enterprise Standard or Plus license and that [AI developer tools are enabled](https://docs.cloud.google.com/gemini/enterprise/docs/ai-developer-tools-settings#enable-ai-developer-tools) for your region (`global`, `us`, or `eu`).
*   **Missing BYOID sign-in option**: Ensure you are running the latest release of the **[Antigravity App](/download)**, the **[Antigravity CLI](/docs/cli/install)**, or your **[IDE extension](/docs/ide/extensions)**, as enterprise authentication and BYOID support are included natively in all recent releases.
*   **TLS certificate or connection errors in `us` or `eu` regions**: Global Private Google Access VIPs (`private.googleapis.com` and `restricted.googleapis.com`) serve a single-level `*.googleapis.com` wildcard certificate that does not match `*.<location>.rep.googleapis.com` subdomains. If connecting to `us` or `eu` fails with an `x509` certificate mismatch or connection error, ensure `businessaicode.googleapis.com` and the regional `*.rep.googleapis.com` domains are allowlisted, and that the regional domains are routed through a Private Service Connect regional endpoint (with `businessaicode.googleapis.com` routed through your global endpoint). See [Private Service Connect (PSC) for regional endpoints](#private-service-connect-psc-for-regional-endpoints).

### Important API provisioning advisory

Caution

**Enable required APIs before purchasing licenses**: New Gemini Enterprise license purchases can fail or fail to provision if the **Gemini Enterprise API** (`aiplatform.googleapis.com`) is not enabled first. Enable the API in the Google Cloud Console and wait approximately five minutes for propagation before completing license purchases.

### Sharing diagnostics with support

When contacting Google Cloud Support, include the diagnostic log file from your most recent session:

*   **Antigravity CLI (Linux and macOS)**:
    
    ```bash
    ~/.gemini/antigravity-cli/cli.log
    ```
    
*   **Antigravity App (macOS)**:
    
    ```bash
    ~/Library/Logs/Antigravity/language_server.log
    ```
    

## What’s next

Explore the following resources to learn more:

*   Explore supported model architectures in the [Models guide](/docs/models).
*   Learn more about enterprise privacy and compliance in [Security and governance](#security-and-governance).
*   Check the [Antigravity CLI reference](/docs/cli/reference) for headless automation commands.