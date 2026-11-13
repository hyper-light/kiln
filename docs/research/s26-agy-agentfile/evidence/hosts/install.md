# Authenticate

Available on: Antigravity App, Antigravity CLI, IDE extensions

Antigravity uses a unified authentication layer across **Antigravity App**, **Antigravity CLI**, **Antigravity for IDEs**, and the **Antigravity SDK**. Signing in on one local surface shares your credentials across your local environment.

Need to install Antigravity first?

Follow the [Quickstart](/docs/getting-started) to download and install **Antigravity App**, **Antigravity CLI**, an **IDE Extension**, or the **Antigravity SDK**.

* * *

## Sign in by surface

Select your surface below to authenticate your account, switch credentials, or configure headless and enterprise environments.

### Antigravity App

#### Personal Google Account

1.  Launch **Antigravity App**.
2.  Click **Sign in** to open the browser authentication flow.
3.  Complete the browser-based OAuth flow using your personal Google Account (Free, Pro, or Ultra plan).
4.  Once authenticated, the browser redirects back to Antigravity App. On first launch (`v2.30.0+`), select what best describes your work (or click **Skip** for the default **Software Engineering** setup) and confirm **Terms & Data Use** to configure your initial [interface layout](/docs/settings#interface-layout) and starter prompts on **Home** (refer to [Get started](/docs/getting-started)).

#### Gemini Enterprise & Google Cloud Account

Google Antigravity uses a single sign-on (SSO) flow for corporate business accounts and automatically detects your assigned Gemini Enterprise license tier:

1.  Start **Antigravity App** and select **Sign in** to open the browser authentication flow.
2.  Choose **Business account** _(subject to the Google Cloud Terms of Service)_.
3.  Select **Continue with Google Cloud** (or select **Advanced WIF Configuration** and enter your organization’s **WIF Configuration String** for Bring Your Own Identity / Workforce Identity Federation).
4.  Complete authentication in your browser.
5.  Once authenticated, the **License Selector** displays your assigned licenses.
6.  Confirm the project linked to your license and select it. Alternatively, select **Other** to self-assign a license by entering your Google Cloud Project ID and selecting a location (`global`, `us`, or `eu`).

Note

**Project Switching & Governance**: To switch to a different Google Cloud project or location, log out of Antigravity App and log back in to select your new project or region. Ensure the **Gemini Enterprise API** (`aiplatform.googleapis.com`) is enabled in your project. See the [Enterprise guide](/docs/enterprise) for full administrator setup, BYOID/WIF, and VPC Service Controls details.

#### Application Default Credentials (ADC)

For automated or headless workflows, you can authenticate Antigravity App using Google Cloud [Application Default Credentials](/docs/enterprise#application-default-credentials-adc):

1.  Generate local Application Default Credentials for your project:
    
    ```bash
    gcloud auth application-default login --project {GCP_PROJECT}
    ```
    
2.  Enable ADC in Antigravity App using either option:
    *   Launch from a terminal with `export AGY_ADC_AUTH=true`.
    *   Or enable the `enableAdc` user setting in `~/.gemini/config/config.json`:
        
        ```json
        {
          "userSettings": {
            "enableAdc": true
          }
        }
        ```

### Antigravity CLI

Antigravity CLI (`agy`) uses secure credentials and token profiles to communicate with the shared agent harness.

#### Local silent keyring sign-in

When launching `agy` on your local machine, the CLI attempts to access your operating system’s native secure keyring (such as Apple Keychain, Linux Secret Service/dbus, or Windows Credential Manager). If a valid token profile is found, the CLI authenticates your session silently without opening a browser.

If no saved session is found:

1.  The CLI automatically launches your local default web browser.
2.  Sign in using your approved account credentials (personal Google Account, or choose **Business account** → **Continue with Google Cloud** / **Advanced WIF Configuration** for [Gemini Enterprise](/docs/enterprise)).

#### Remote SSH OAuth flow

When running over SSH, the CLI detects the remote connection environment. Because it cannot launch a local web browser, the CLI initiates a manual URL loop:

1.  Launch `agy` in your remote terminal session.
2.  The CLI detects the SSH environment and prints a unique, secure authorization URL.
3.  Copy this URL and paste it into a web browser on your local machine.
4.  Sign in with your approved credentials and complete the authentication.
5.  The browser displays a unique alphanumeric authorization code.
6.  Copy this code, return to your remote SSH terminal, and paste it into the prompt.

#### Using a Gemini API key

Run Antigravity CLI with your own Gemini API key instead of a signed-in Google account. Model requests go directly to the Gemini API, and the CLI never establishes an account session. This suits headless and CI runs, where no browser is available to complete a sign-in. Create a key in [Google AI Studio](https://aistudio.google.com/app/api-keys).

To use a Gemini API key, you have to set a provider and an environment variable with the API key. Only setting a `GEMINI_API_KEY` environment variable on its own has no effect.

##### Enable the Gemini API key

1.  Set `modelProvider` to `gemini` in `~/.gemini/antigravity-cli/settings.json`:
    
    ```json
    {
      "modelProvider": "gemini"
    }
    ```
    
2.  Export your key as `GEMINI_API_KEY`:
    
    ```bash
    export GEMINI_API_KEY="your-api-key"
    ```
    
    This applies to the current shell only. Add the same line to your shell profile, such as `~/.zshrc` or `~/.bashrc`, to persist it across sessions.
    
3.  Start the CLI:
    
    ```bash
    agy
    ```
    

The CLI skips the sign-in screen and opens the main interface directly. The header shows **Gemini API key** instead of your account email:

![Antigravity CLI authenticated with a Gemini API key, with "Gemini API key" shown in the header in place of an account email](/assets/image/docs/cli/install-gemini-api-key.png)

> **Note:** When you use the authentication with a `GEMINI_API_KEY`, `/logout` has no effect because there is no stored session to clear.

##### Point the CLI to a custom endpoint

To send model requests to a different Gemini-compatible endpoint, set the `GOOGLE_GEMINI_BASE_URL` environment variable:

```bash
export GOOGLE_GEMINI_BASE_URL="https://your-endpoint.example.com"
```

##### Revert to default authentication

If you want to revert back to using the default account based authentication:

1.  Remove `modelProvider` from `~/.gemini/antigravity-cli/settings.json`.
2.  Restart the CLI to sign in to your account.

> **Note:** The CLI cannot start if you unset the `GEMINI_API_KEY` environment variable, but still have the `modelProvider` set to `gemini`.

##### Troubleshooting API key authentication

| Symptom | Cause | Resolution |
| :-- | :-- | :-- |
| The CLI exits at startup reporting that `GEMINI_API_KEY` is not set | `modelProvider` is `gemini` but the key is missing from the environment | Export `GEMINI_API_KEY`, or remove `modelProvider` to use the default authentication |
| The CLI signs in normally and ignores the setting | `modelProvider` holds an unrecognized value | `gemini` is the only accepted value. Check the spelling and restart the CLI |
| A key set through `GOOGLE_API_KEY` or a `.env` file has no effect | The CLI reads the credential only from `GEMINI_API_KEY` in the environment, and does not load `.env` files | Export `GEMINI_API_KEY` in your shell or shell profile |
| Requests fail mid-session with a generic model error | The key is invalid, revoked, or lacks access to the requested model | Verify the key in Google AI Studio. The CLI checks only that the key is non-empty at startup, so an unusable key only surfaces on the first conversation |

#### Application Default Credentials (ADC)

For headless enterprise environments and automated terminal workflows, Antigravity CLI supports Google Cloud [Application Default Credentials](/docs/enterprise#application-default-credentials-adc) (ADC):

1.  Generate local ADC credentials for your project:
    
    ```bash
    gcloud auth application-default login --project {GCP_PROJECT}
    ```
    
2.  Enable ADC in the CLI by setting `AGY_ADC_AUTH=true` (and optionally `GOOGLE_CLOUD_QUOTA_PROJECT` and `GOOGLE_CLOUD_LOCATION`):
    
    ```bash
    export AGY_ADC_AUTH=true
    ```
    
3.  To sign out of ADC on the CLI, unset the environment variable (`unset AGY_ADC_AUTH`) and restart your terminal session.

#### Managing your session (logging out)

Terminating your session clears active credentials and local cache directories. To disconnect your account and purge saved authentication profiles from your operating system’s keyring, run the following command in the CLI prompt box:

```text
/logout
```

#### Installation flags

When executing the CLI installation scripts (see [Quickstart](/docs/getting-started?tab=cli#download-and-install)), you can append the following customization flags:

*   `--skip-aliases`: Bypasses shell profile alias purging (prevents the script from purging or updating legacy `agy` or `antigravity` shell aliases).
*   `--skip-path`: Bypasses shell profile `PATH` appending (prevents the script from modifying your shell profile’s dynamic environment variables).

### For IDEs

#### Unified sign-in across IDE extensions

Antigravity uses a unified authentication system across all [IDE extensions](/docs/ide/extensions) ([Visual Studio Code](/docs/ide/extensions/vscode), [IntelliJ-based IDEs](/docs/ide/extensions/jetbrains), [Visual Studio](/docs/ide/extensions/visual-studio), [Zed](/docs/ide/extensions/zed), [Xcode](/docs/ide/extensions/xcode), and [Eclipse](/docs/ide/extensions/eclipse)), the Antigravity CLI, and Antigravity App.

*   **Individual developers**: Click the Antigravity icon in your editor’s activity bar or tool window (or select **Antigravity** and use the **…** button under **Xcode** > **Settings** > **Intelligence** in Xcode) and click **Sign In** to complete authentication in your default web browser.
*   **Enterprise teams & organizations**: Click **Sign In** and select **Sign in with Gemini Enterprise** (or choose **Business account** → **Continue with Google Cloud** / BYOID), then select your target **Google Cloud Project** and **Region** (`global`, `us`, or `eu`). Gemini Enterprise authentication for JetBrains IDEs, Zed, Xcode, and Eclipse is currently in **Preview**.

#### Agent configuration (`auth.type`)

In editors that configure external agents via JSON settings ([IntelliJ-based IDEs](/docs/ide/extensions/jetbrains), [Zed](/docs/ide/extensions/zed), and [Xcode](/docs/ide/extensions/xcode)), configure your authentication method using the following settings:

### Google Accounts

For individual Google AI subscription plans (including Free, Pro, and Ultra):

```json
{
  "auth": {
    "type": "oauth-personal"
  }
}
```

### Gemini Enterprise

Reach out to your cloud administrator for your GCP Project ID and Region (Enterprise support for JetBrains, Zed, Xcode, and Eclipse is currently in Preview):

```json
{
  "auth": {
    "type": "oauth-business"
  },
  "gcp": {
    "project": "<YOUR_GCP_PROJECT_ID>",
    "location": "<YOUR_GCP_REGION>"
  }
}
```

### Gemini API

Authenticate using a Gemini API key:

```json
{
  "auth": {
    "type": "gemini-api-key"
  }
}
```

### API / ADC

Authenticate with Gemini Enterprise using an API Key or Application Default Credentials (ADC) (Preview). When using ADC, specify your GCP Project ID and Region:

```json
{
  "auth": {
    "type": "agent-platform"
  },
  "gcp": {
    "project": "<YOUR_GCP_PROJECT_ID>",
    "location": "<YOUR_GCP_REGION>"
  }
}
```

### Antigravity SDK

#### Programmatic authentication

The **Google Antigravity SDK** (`google-antigravity`) supports the following authentication methods for Python 3.10+ applications (see [SDK Overview](/docs/sdk/overview)):

1.  **Gemini API Key**: Set your Gemini API key in the environment before initializing `Agent`:
    
    ```bash
    export GEMINI_API_KEY="your_api_key_here"
    ```
    
2.  **Gemini Enterprise (Google Cloud / Vertex AI)**: Set `vertex=True` in `LocalAgentConfig` alongside your GCP `project` and `location` to route requests through the Gemini Enterprise API (`aiplatform.googleapis.com`):
    
    ```python
    from google.antigravity import Agent, LocalAgentConfig
    
    config = LocalAgentConfig(
        vertex=True,
        project="your-gcp-project",
        location="us-central1",
    )
    ```
    
    Or configure Gemini Enterprise via environment variables and Application Default Credentials:
    
    ```bash
    export GOOGLE_GENAI_USE_VERTEXAI=True
    export GOOGLE_CLOUD_PROJECT="your-gcp-project"
    export GOOGLE_CLOUD_LOCATION="us-central1"
    gcloud auth application-default login
    ```
    
3.  **Local models (offline, no API key required)**: Run agents directly on-device using `LiteRTAgentConfig` or connect to a local OpenAI-compatible server (`Ollama`, `LM Studio`, `vLLM`) using `LocalOpenAIAgentConfig`. See [Local models](/docs/sdk/local-models).

* * *

## Next steps

[

Get Started

Quickstart

Install Antigravity and complete your first end-to-end agent workflow.

](/docs/getting-started)[

Models

Choose a model

Configure Gemini 4 Carbon, Gemini 4 Argon, Gemini 4 Hydrogen, Gemini 3.8 Flash, Gemini 3.1 Pro, Claude, and reasoning effort.

](/docs/models)[

Cloud & APIs

Build with Google

Connect Vertex AI, Cloud Run, Firebase, and Google Cloud MCP tools.

](/docs/build-with-google)