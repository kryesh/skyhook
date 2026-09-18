# Skyhook

Skyhook is a provider-neutral coding-agent harness with a programmable JavaScript orchestration
runtime. A tool is registered once and is then available through the model tool protocol and as a
lazy builder inside `script`.

The `skyhook-agent` package contains the `skyhook` library crate and the `skyhook` CLI binary.
The library does not depend on a
provider-specific response type; native backends implement OpenAI Chat Completions and Responses,
Anthropic Messages, and Codex/ChatGPT subscription behind a common provider API.

This book covers installation, everyday use, configuration, scripting, the tool contracts,
and development of Skyhook. Start with [installation](getting-started/installation.md) and
[your first session](getting-started/first-session.md), or explore the
[JavaScript workflow runtime](scripting/introduction.md).

The source repository is [github.com/kryesh/skyhook](https://github.com/kryesh/skyhook).
The planned canonical documentation URL is <https://kryesh.github.io/skyhook/>;
publication is a separate step from building this book locally.

Skyhook is licensed under [AGPL-3.0-only](https://github.com/kryesh/skyhook/blob/main/LICENSE).
