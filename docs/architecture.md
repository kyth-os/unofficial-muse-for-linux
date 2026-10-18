# Architecture

## Separation of concerns

- `muse-linux-core`: permission-scoped filesystem operations and policy tests. It remains independent of the GUI and network stack.
- `muse-meta-api`: adapter for Meta’s documented Responses API. It sends `store:false` and requests encrypted reasoning content so the app can replay reasoning locally without server-managed conversation state.
- `muse-agent-runtime`: bounded function-tool loop over typed read-only local tools. It preserves encrypted response items between calls, scopes file operations to the current grant, and resets prior context when file-sharing permission is reduced or the folder grant changes.
- `apps/desktop`: native GTK4 shell with session-only developer-key entry, explicit paid-use consent, separate filename/content sharing controls, assistant conversation, and local folder browsing/preview.
- Local connectors: narrow capabilities, each with its own grant and audit-friendly result. Never expose a general-purpose shell or unrestricted filesystem API to the assistant.

## Muse service backend and account compatibility

The target is a Linux-native port of the Mac agent’s user-facing behavior, not an integration with a Mac host or installed Mac app. Linux filesystem, browser, screen, and application access must be implemented through Linux APIs and explicit grants. The prototype now has a non-streaming assistant loop and a bounded, read-only local file-tool path, but it is not feature-complete and the API path has only been tested against local mocks. Meta’s [Muse for Productivity](https://ai.meta.com/muse/productivity/) page describes the Mac app’s permissioned access to files, Messages, Calendar, and Notes. Consumer Muse sign-in and same-account continuity remain unimplemented and unverified; they are separate from Linux host parity.

Use only documented Meta/Muse service interfaces for a Muse-backed assistant. Keep backend authentication separate from OS permissions, and never reuse browser cookies or internal, unversioned routes. The [Meta Model API](https://dev.meta.ai/docs/api-reference) is an official developer API authenticated with a Model API key; it is not the existing consumer account and may have separate billing. [Muse Code](https://dev.meta.ai/docs/muse-code) is Meta’s Linux terminal/CI coding agent, not a client protocol. The [Muse Connector Platform](https://platform.muse.ai/docs) lets Muse call reviewed service endpoints; it is not a pairing flow or a replacement for native Linux integrations.

Any future model-service adapter must be replaceable, opt-in, disclose the active service and billing boundary, and keep credentials out of the repository and logs. Do not claim same-account compatibility until a documented flow is implemented and tested.

## Linux desktop boundary

Use XDG desktop portals for file selection, screen capture, and remote desktop/input where supported; use accessibility and app-specific provider APIs for semantic context. AppImage packages the program, not the user’s desktop session services. Handle missing portals or provider APIs as explicit capability gaps.

## Dependency and asset policy

Implement the client from scratch. Do not copy Meta’s proprietary binaries, source, UI assets, hidden client tokens, or private service credentials into this repository. Retain the project’s GPL-3.0-only license and the non-affiliation notice.
