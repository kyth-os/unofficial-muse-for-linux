# Architecture

## Separation of concerns

- `muse-linux-core`: permission-scoped filesystem operations and policy tests. It remains independent of the GUI and network stack.
- `muse-meta-api`: experimental adapter for Meta’s documented developer Responses API. It is not Muse.ai account authentication and does not reach the user's existing personal agent or history.
- `muse-agent-runtime`: bounded function-tool loop over typed read-only local tools for that experiment. It is not yet the local capability bridge for a signed-in Muse.ai session.
- `apps/desktop`: native GTK4 dark shell with Chats/Files views; current assistant setup still exposes a session-only developer key and is not the intended Muse.ai sign-in/pairing flow.
- Local connectors: narrow capabilities, each with its own grant and audit-friendly result. Never expose a general-purpose shell or unrestricted filesystem API to the assistant.

## Muse.ai account link and local-host bridge are core requirements

The target is a Linux-native companion/local host for the user's existing Muse.ai personal agent. It must pair through a documented Muse account/device flow permitted for the intended distribution, preserve the same identity and task/chat continuity, and provide Linux filesystem, browser, screen, and app access through explicit Linux grants. The account link and local host bridge are both required parts of the product—not optional compatibility work.

The current [Meta Model API](https://dev.meta.ai/docs/api-reference) adapter is a separate developer-key experiment; it must not be presented as the user's Muse.ai login or same-agent experience. Meta also publishes a [Linux Device SDK](https://github.com/facebookincubator/muse-gadget-sdk/tree/main/linux) that pairs a Linux gadget to a Muse account and lets Muse run commands and access files on the host. Its documentation and [token terms](https://gadgets.muse.ai/sdk-terms) explicitly limit it to personal, non-commercial use, say it is unsupported and may change or stop, and restrict publicly offered token-bearing devices. Whether a public AppImage that asks each user for their own token is permitted is unclear and needs Meta confirmation. It is a relevant account-linked Linux path to evaluate for a personal proof of concept, but it is a phone-paired gadget service, not automatically a Mac-style desktop client. Open [issue #154](https://github.com/facebookincubator/muse-gadget-sdk/issues/154) reports that app-to-device command dispatch is absent for at least one account; later reports say behavior may vary by account. Test the user's own account before treating pairing as functional. Confirm the authorized flow, scopes, terms, and UX before adopting it. Do not reuse browser cookies, private routes, hidden endpoints, or internal session data. If no permitted flow fits the project, treat account pairing as a P0 blocker rather than substituting the developer API.

Keep Muse account authentication separate from Linux OS permissions: Muse sign-in/device pairing authorizes the account; local portals/connectors authorize access to the Linux machine. Verify account identity, revocation, shared history/tasks, and per-action permission behavior end to end before claiming parity. [Muse Code](https://dev.meta.ai/docs/muse-code) and the [Muse Connector Platform](https://platform.muse.ai/docs) are not by themselves proof of a desktop-client pairing protocol.

Any experimental developer-API adapter must be clearly labeled, replaceable, and kept out of the product's account-link path. The product flow must use documented Muse.ai account/device authorization and keep all credentials out of the repository and logs.

## Linux desktop boundary

Use XDG desktop portals for file selection, screen capture, and remote desktop/input where supported; use accessibility and app-specific provider APIs for semantic context. AppImage packages the program, not the user’s desktop session services. Handle missing portals or provider APIs as explicit capability gaps.

## Dependency and asset policy

Implement the client from scratch. Do not copy Meta’s proprietary binaries, source, UI assets, hidden client tokens, or private service credentials into this repository. Retain the project’s GPL-3.0-only license and the non-affiliation notice.
