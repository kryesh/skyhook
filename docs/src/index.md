# Skyhook

Skyhook is a provider-neutral coding agent with a terminal interface and a programmable
JavaScript workflow runtime. Use it interactively, automate work in headless sessions,
or embed its Rust library in your own application.

Skyhook supports OpenAI Chat Completions and Responses, Anthropic Messages, Codex/ChatGPT
subscriptions, and compatible local model endpoints. Tools work in local workspaces and
on SSH targets. The agent can call tools directly or use scripts to coordinate jobs and
child agents through the same permissions and saved-output system.

## Find your starting point

- **New users:** [install Skyhook](getting-started/installation.md) and
  [start your first session](getting-started/first-session.md).
- **Everyday use:** learn the [terminal interface](guide/terminal-interface.md),
  [session management](guide/sessions-and-context.md), and
  [permissions](guide/permissions.md).
- **Configuration:** choose [providers and models](configuration/providers-and-models.md)
  and set up [authentication](configuration/authentication.md).
- **Automation:** run [headless workflows](guide/headless.md) or use
  [JavaScript scripts](scripting/introduction.md).
- **Development:** see [setup](development/setup.md),
  [architecture](development/architecture.md), and [embedding](development/embedding.md).

Source: [github.com/kryesh/skyhook](https://github.com/kryesh/skyhook).
Skyhook is licensed under [AGPL-3.0-only](https://github.com/kryesh/skyhook/blob/main/LICENSE).
