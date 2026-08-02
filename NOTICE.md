# NOTICE

**Medley** is a community-maintained fork of the open-source **Grok Build**
terminal coding agent. It is distributed under the Apache License, Version 2.0
— see [`LICENSE`](LICENSE).

## Attribution

This product includes software developed by xAI as part of Grok Build
(<https://github.com/xai-org/grok-build>), licensed under the Apache License,
Version 2.0.

Medley is maintained at <https://github.com/ImL1s/grok-build>. Its changes
relative to upstream live on the `providers` branch and are summarised in
[`FORK.md`](FORK.md), which serves as the Apache-2.0 §4(b) statement of
modification. The [`SOURCE_REV`](SOURCE_REV) file records the upstream commit
this tree was synced from.

## Trademarks

Medley claims no rights in any third-party mark.

- **"Grok", "Grok Build", "xAI", "SpaceXAI", and the associated logos** are
  trademarks of their respective owners. The Apache-2.0 license that covers the
  upstream source grants **no** trademark rights (§6). Medley uses these names
  only descriptively — to identify the upstream project this fork is derived
  from, and to describe compatibility. This is not a claim of ownership,
  affiliation, or origin.
- **"OpenAI", "ChatGPT", and "Codex"** are trademarks of OpenAI. Medley uses
  them only to identify the third-party service a user may choose to connect to.
- **"Anthropic", "Claude", "Google", "Gemini", "Ollama", "LM Studio", "vLLM",
  "llama.cpp", "OpenRouter", "Together AI"** and other provider names appearing
  in the configuration examples are trademarks of their respective owners and
  are used the same way.

## Non-affiliation

> **Medley is not affiliated with, endorsed by, sponsored by, certified by, or
> supported by xAI.** It is an independent fork published by third-party
> maintainers. xAI provides no warranty, support, or security response for this
> distribution. Do not report Medley bugs or vulnerabilities to xAI, and do not
> represent Medley builds as an official Grok Build release.

The inherited [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`SECURITY.md`](SECURITY.md)
files describe **upstream's** policies for the official project. They do not
describe how this fork accepts reports. Use the fork's own issue tracker at
<https://github.com/ImL1s/grok-build/issues>.

## Third-party service boundary (OpenAI Codex)

Medley can sign in to OpenAI Codex with a ChatGPT account and route requests to
`https://chatgpt.com/backend-api/codex/responses`. That capability is
**compatibility with a pinned public Codex contract, not an OpenAI Platform API,
not a guaranteed-stable public interface, and not an OpenAI endorsement of this
client.**

- The OAuth consent page identifies the **registered Codex client**, not Medley.
  Review it before granting access.
- Availability, entitlements, workspace policy, rate limits, and model access
  remain governed by your OpenAI account and OpenAI's terms of use. Medley
  cannot grant, extend, or guarantee any of them.
- Medley does not impersonate the official Codex CLI. Its Codex credential is
  stored under its own provider scope and never merges with, replaces, or reads
  a credential owned by the official Codex CLI.
- The contract can change without notice on OpenAI's side; when it does, this
  transport can break until the fork is updated.

Nothing in this repository is legal advice. Operators are responsible for their
own compliance with each provider's terms.

## Other notices

Third-party and vendored code remains under its original licenses:

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) — crates.io / git dependencies,
  bundled UI themes, and in-tree source ports (including openai/codex and
  sst/opencode tool implementations)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
  — crate-local notice for the codex and opencode ports (license texts plus the
  Apache §4(b) change notice)
- [`third_party/NOTICE`](third_party/NOTICE) — vendored Mermaid-stack index
