# my-google-cli

A single-binary Rust CLI (`mg`) for Gmail built on the Gmail REST API. It is agent-first, with readable human output, and follows the `hey` CLI command style.

## Status: PLANNING ONLY
- No code exists yet. The full design is in `PLAN.md`; read it before doing anything.
- Do not implement, scaffold, install toolchains, or run `cargo` until the user explicitly asks for a specific milestone (e.g. "do M1").
- Treat `PLAN.md` as the source of truth. Propose changes to it; don't silently deviate.

## Working rules
- Take no action the user hasn't explicitly asked for. Questions like "how/why/what" get an explanation, not changes.
- Be brief and precise. No emoji. Reply in the language the user wrote in (Polish or English).
- Prefer the simplest solution with the fewest moving parts.
- Never commit or push unless asked.

## Fixed decisions (see PLAN.md)
- **Language and dependencies:** Rust, blocking I/O (`ureq` + rustls), no async runtime, no OpenSSL. One static-ish binary.
- **Scope:** Gmail only, scope `gmail.modify`. No Calendar/Drive.
- **Account:** single account. Keyring entries are still keyed by email address, so multiple accounts can be added later.
- **Token storage:** refresh token in the OS keyring (Secret Service) by default. Headless opt-in only: `mg auth login --token-file` stores it in `~/.config/my-google-cli/refresh_token` (0600, checked on read). Never fall back to the file silently. Access token is held in memory. Client JSON lives at `~/.config/my-google-cli/` (dir 0700, files 0600).
- **OAuth app:** "In production", unverified. Flow uses PKCE S256, random state, a 127.0.0.1 loopback redirect, and a 5-minute timeout.
- **Output:** compact JSON when piped, text in a TTY; `--json`/`--text` override.
- **Toolchain:** Rust via mise (`mise use -g rust`).
- **Distribution:** local build only (`cargo build --release`, copy to `~/.local/bin`). No CI or releases.

## Security invariants (never break these)
- **Network:** talk only to the hardcoded hosts `gmail.googleapis.com`, `oauth2.googleapis.com`, `accounts.google.com`. No telemetry, no update checks, no other hosts.
- **Sending:** agents can only create drafts. `mg draft send` must require a real TTY confirmation (read from `/dev/tty`, user types `send`). There is no `--yes`/force flag, ever.
- **Output sanitizing:** strip C0/C1 control characters and ANSI/OSC escapes from all text output.
- **Input validation:** check message, thread and attachment ids against `[A-Za-z0-9_-]+` before they go into URLs.
- **Attachments:** sanitize filenames (basename only, safe characters), never overwrite existing files, print saved paths and never the file bytes.
- **Composing:** build outgoing mail with `mail-builder`, never by concatenating header strings.
- **Secrets:** never log or print tokens or the client secret.

## Commands (once code exists)
- Build: `cargo build --release`
- Test: `cargo test`
- Lint: `cargo clippy -- -D warnings`
- Audit: `cargo audit`

## Reference
- **Prior art:** `../gmail-cli` (Bun/TS, third-party; security-reviewed, untrusted). Port its logic, not its code:
  - `src/auth/oauth.ts`
  - `src/gmail/message.ts` (`walkParts`, `buildReplyHeaders`, `pickSendAs`)
  - `src/google/api.ts` (retry/backoff)
  - `src/cli/commands/attachments.ts` (`safeFilename`)
- **Known weaknesses of gmail-cli that must not be repeated:** plaintext token file, unsanitized terminal output, unvalidated account name in file paths.
