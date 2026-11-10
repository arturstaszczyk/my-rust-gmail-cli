# Plan: `my-google-cli`, a single-binary Rust Gmail CLI for agents

## Status (2026-11-10)
- **M1: code complete, not yet tested end-to-end.** `cargo test` (39 tests) and `cargo clippy --all-targets -- -D warnings` pass, and the release build works. Nothing has run against the live Gmail API yet: that needs the one-time setup below plus `mg auth login`.
- **M2, M3:** not started.
- **Toolchain:** Rust 1.99.1 via mise. The repo is local git (`master`, no remote) with no commits yet.
- **Not run yet:** `cargo audit`, because `cargo-audit` is not installed (`cargo install cargo-audit`).
- **To check on the first live run:**
  - Partial-response `fields` mask in `search`.
  - Loopback login in a real browser.
  - Charset handling of real ISO-8859-2 mail.

## Context
The claude.ai Gmail connector can't download attachments. The third-party `gmail-cli` (Bun/TS) works but isn't trusted and needs Bun. Goal: our own single-binary Rust CLI on the Gmail REST API, built agent-first with readable human output, and modelled on the `hey` CLI command style. The first concrete use case: find an email and save its PDF attachment to disk so the agent can read it.

Decisions made:
- **Scope:** Gmail read + attachments, organize, drafts/send. No Calendar/Drive.
- **Output:** agent-first (compact JSON when piped, text in a TTY).
- **Token storage:** OS keyring.
- **Sending:** agents create drafts only; actually sending needs a human at a TTY.
- **OAuth app:** "In production", unverified (refresh token doesn't expire after 7 days).
- **Accounts:** single account.
- **Toolchain:** Rust via mise.
- **Distribution:** local build only.

Output dir: `/home/artur/Work/Projects/my-google-cli`. Binary name: `mg` (easy to rename).

## Dependencies
No async runtime, no OpenSSL. Pure-Rust TLS.
- `clap` (derive): CLI
- `ureq` 3 + rustls: blocking HTTPS
- `serde`, `serde_json`
- `keyring` 3 (`sync-secret-service`, `crypto-rust`, `vendored`): Secret Service (gnome-keyring/KeePassXC), dbus vendored so no runtime lib needed
- `sha2`, `getrandom`, `base64`: PKCE, state, base64url
- `mail-builder`: RFC 5322 composition (proper header encoding, prevents CRLF header injection). Added in M3.
- `html2text`: HTML bodies to text
- `encoding_rs`: non-UTF-8 charsets (ISO-8859-2 is common in Polish mail; the Gmail API returns part bytes in their original charset)
- ~~`directories`~~: dropped. The config path is `$XDG_CONFIG_HOME` (if absolute) or `~/.config` via `std::env`, so the crate isn't needed.
- Loopback redirect: plain `std::net::TcpListener` on 127.0.0.1:0, no HTTP server crate

As built in M1 (pinned in `Cargo.toml` / `Cargo.lock`):
- Versions: clap 4.6, ureq 3.5 (rustls + ring, webpki-roots), keyring 3.6, sha2 0.11, getrandom 0.4, base64 0.23, html2text 0.17, encoding_rs 0.8.
- keyring 4.x exists but has a different API; we stay on 3 for now.
- `cargo tree` has no OpenSSL and no zbus. dbus is vendored.

Release profile: `lto = true`, `strip = true`, `codegen-units = 1`, `panic = "abort"`.

## Layout
```
my-google-cli/
  Cargo.toml
  src/main.rs           clap definitions, dispatch, usage-error handling
  src/error.rs          one error type; kind -> exit code (1/2/3)
  src/output.rs         JSON/text switch, sanitizing, truncation, tables
  src/confirm.rs        TTY gate (reads /dev/tty; refuses when no TTY)       [M3]
  src/auth/mod.rs       auth setup/login/logout/status commands
  src/auth/oauth.rs     PKCE + loopback flow, token exchange/refresh/revoke
  src/auth/store.rs     keyring (refresh token) + config files (client id/secret, account)
  src/api.rs            HTTP client: bearer auth, 429/5xx backoff, host lockdown
  src/gmail/mod.rs      API types, id validation, get_thread, dates, entities
  src/gmail/{search,thread,attachment,mime}.rs                               [M1]
  src/gmail/{label,draft}.rs                                                 [M2, M3]
  tests/fixtures/*.json Gmail-API-shaped messages and a thread for unit tests
```

Notes on the layout:
- **Fixtures:** they are hand-built in the Gmail API response shape, not recorded. Replace or extend them with real (scrubbed) responses once login works.
- **Unit tests:** they live next to the code (`#[cfg(test)]`). The binary-only crate has no `tests/*.rs` integration tests.
- **Config files:** `client.json` holds `{client_id, client_secret}`. `config.json` holds `{account}`, the email used as the keyring key. Both are 0600, written atomically, in a 0700 dir.

Port the logic (not the code) from the reviewed `gmail-cli`:
- `src/auth/oauth.ts`: PKCE S256, state check, loopback flow, `access_type=offline` + `prompt=consent`
- `src/gmail/message.ts`: `walkParts` (MIME tree), `buildReplyHeaders` (In-Reply-To/References/Re:), `pickSendAs`
- `src/google/api.ts`: retry/backoff policy
- `src/cli/commands/attachments.ts`: `safeFilename`

## Commands (hey-style)
| Command | Notes |
|---|---|
| `mg auth setup --client-json <path>` | Imports the Desktop OAuth client JSON to `~/.config/my-google-cli/client.json` (0600, dir 0700) |
| `mg auth login / logout / status` | Login opens browser; logout also revokes the token at Google |
| `mg search "<gmail query>" [--limit N]` | Thread summaries: id, date, from, subject, snippet, attachment count |
| `mg box inbox\|unread\|starred\|sent` | Shortcuts for common searches |
| `mg thread <id> [--full] [--max-chars N]` | Plain-text bodies (HTML converted), quoted history stripped unless `--full`, default 4000 chars per message |
| `mg attachment list <thread-or-msg-id>` | |
| `mg attachment save <msg-id> [<part>] --out <dir>` | Selects by MIME part id from `attachment list`, because Gmail attachment ids change on every fetch. An attachment id is still accepted. Sanitized filenames, never overwrites (adds `-1`, `-2`), files are 0600, prints absolute saved paths only, never the bytes |
| `mg label list\|add\|remove`, `mg archive`, `mg seen`, `mg unseen`, `mg trash`, `mg spam` | Accept multiple ids |
| `mg compose / reply / forward` | Always create a **draft**; output the draft id + a summary |
| `mg draft list / show / delete` | |
| `mg draft send <id>` | **Human gate:** prints recipients/subject/body preview on `/dev/tty` and requires typing `send`. No `--yes` flag. Fails with no TTY, so an agent (Claude Code's Bash has no TTY) can't send |

Output format:
- **TTY:** aligned text.
- **Piped:** compact JSON.
- **Overrides:** `--json` / `--text`.
- **Errors:** sent to stderr as `{"error":..., "hint":...}`, with exit codes 1 (usage), 2 (auth), 3 (API).

As built in M1:
- **Error format:** errors follow the output mode: JSON when piped, `error:` / `hint:` lines in a TTY.
- **Exit code 1:** also covers local failures (files, directories), and clap's own usage errors use it too.
- **Dates:** ISO 8601 UTC from `internalDate`. Tables show `YYYY-MM-DD HH:MM` (UTC); there is no local time zone yet.
- **`box`:** `inbox` = `in:inbox`, `unread` = `in:inbox is:unread`, `starred` = `is:starred`, `sent` = `in:sent`.
- **`search`:**
  - Calls `threads.list`, then `threads.get` per thread, 8 in parallel.
  - The `threads.get` calls use a `fields` mask, so bodies are never downloaded.
  - The listing shows the first message's sender and subject and the last message's date.
- **`thread`:**
  - Accepts a message id too, resolving it to its thread.
  - Quoted history is cut at "On … wrote:" / "W dniu … napisał(a):" / "-----Original Message-----" and at `>` lines.
- **`auth status`:** does a real token refresh to report `logged_in`.

## Security design
- **Scope:** `gmail.modify` only. It covers read, labels, trash, drafts and send. Permanent delete and settings (filters, forwarding) can't be reached by design.
- **Tokens:**
  - The refresh token is kept in the keyring only.
  - The access token is held in memory only. Each run refreshes it, which is cheap.
  - The client id/secret go in the config file; a desktop client secret isn't really secret.
- **OAuth:** PKCE S256, random state, 127.0.0.1 bind, 5-minute timeout, single request then close.
- **Sanitizing:**
  - C0/C1 control characters and ANSI/OSC escapes are stripped from all text output.
  - JSON is escaped by serde.
  - This fixes the weakness found in `gmail-cli`.
  - As built:
    - One choke point (`output::emit`/`emit_error`) sanitizes all text output, and every string in JSON output too.
    - It also strips bidi override/isolate characters.
    - It turns a lone CR into LF.
    - An unterminated OSC swallows the rest of the string.
- **Input checks:** message ids and attachment ids are validated against `[A-Za-z0-9_-]+` before going into URLs.
- **Hosts:** only `gmail.googleapis.com`, `oauth2.googleapis.com`, `accounts.google.com` and `oauth2.googleapis.com/revoke`, hardcoded. As built, the ureq agent is set to HTTPS only, follows no redirects, and ignores the `*_PROXY` env vars, so traffic can't be steered elsewhere.
- **Loopback login:**
  - Requests other than `GET /` (favicon, idle browser preconnects) get a 404 and the listener keeps waiting.
  - The first `GET /` decides the result.
  - The response page is static and never echoes the code or other input.
- **Attachment names:**
  - Allowed characters: letters/digits (Unicode), `.`, `-`, `_`. Anything else, including `/` and `\`, becomes `_`. So `Faktura 11/2026.pdf` -> `Faktura_11_2026.pdf`, always a plain basename.
  - No leading `.`/`-`, at most 120 characters (the extension is kept), `attachment` if nothing is left.
  - Files are created with `create_new`, which never overwrites a file and never follows a symlink.
- **Logging:** no telemetry, no update checks, nothing written except config, keyring and saved attachments.

## Weak sides (accepted or mitigated)
1. **The draft-only rule is enforced by the CLI, not by Google.** `gmail.modify` can send. If the refresh token leaks, an attacker can send mail as you. There's no Google scope that allows drafts but blocks sending.
2. **The keyring doesn't isolate apps.** On an unlocked session, any process running as you can ask Secret Service for the token. It protects against plain-file leaks (backups, dotfile repos, `cat`), not against malware running as your user.
3. **The unverified production app** shows a warning at every login. Google could tighten this policy later.
4. **Prompt injection:** email content flows into the agent's context. The draft gate limits the damage; it can't prevent the agent from being misled when reading.
5. **Single account:** your mail for other addresses won't be reachable. Keying keyring entries by email keeps a later `--account` option cheap.
6. **Messy real-world mail:** charsets, broken MIME, giant HTML newsletters. Mitigated with `encoding_rs`, truncation and fixture tests, but there will be edge cases.
7. **Quotas:** the Gmail API has per-user rate limits. Use `threads.get` instead of per-message calls, with backoff on 429.
8. **Maintenance:** you own it. Dependency updates come down to `cargo update` + `cargo audit` now and then.

## Milestones
1. **M1, today's need:** project skeleton, auth (setup/login/status/logout), `search`, `thread`, `attachment list/save`, output and sanitizing. **Code complete 2026-11-10; end-to-end test pending.**
2. **M2:** labels, archive, seen/unseen, trash, spam.
3. **M3:** compose/reply/forward as drafts, `draft list/show/delete`, gated `draft send`.

## One-time setup (yours, about 5 minutes)
1. Install the toolchain: `mise use -g rust`.
2. Google Cloud console: create a project and enable the Gmail API.
3. Set up the OAuth consent screen: External, add the `gmail.modify` scope, then click **Publish app** (move it to production).
4. Create credentials: OAuth client ID, type **Desktop app**, then download the JSON.
5. Run `mg auth setup --client-json ~/Downloads/client_secret_*.json`, then `mg auth login`. Click through the "unverified app" warning once.

Install and uninstall:
- Install: `cargo build --release && cp target/release/mg ~/.local/bin/`.
- Uninstall: `rm ~/.local/bin/mg; rm -rf ~/.config/my-google-cli`, remove the keyring entry (`secret-tool clear service my-google-cli`), and revoke access at myaccount.google.com/permissions.

## Verification
- `cargo test`: PKCE vector, MIME walk on fixtures (multipart, nested, ISO-8859-2, HTML-only), reply headers, filename sanitizing, control-char stripping, id validation.
- `cargo clippy -- -D warnings`, `cargo audit`.

M1 results (2026-11-10):
- [x] `cargo test`: 39 pass.
  - Areas covered: PKCE vector, auth URL, callback parsing, a real loopback listener with noise requests, scope check, OAuth/API error mapping, client JSON import, sanitizing (CSI/OSC/C0/C1/bidi), JSON sanitizing, truncation, id validation, dates, entities, MIME walk (multipart, nested, inline data, ISO-8859-2, HTML-only), quote stripping (EN/PL/wrapped/Outlook), thread and search rendering, filename sanitizing, never-overwrite (incl. symlink), 0600 mode.
  - Reply headers move to M3.
- [x] `cargo clippy --all-targets -- -D warnings`: clean.
- [x] Release binary is 4.8 MB. `ldd` shows libc and libgcc_s only (no libdbus, no libssl).
- [x] CLI smoke tests with an isolated `XDG_CONFIG_HOME`:
  - Errors and exit codes (1/2) are correct.
  - `auth setup` writes a 0600 file in a 0700 dir and rejects a Web client.
  - `auth status` works with nothing configured.
- [ ] `cargo audit`: not run yet, `cargo-audit` is not installed.
- [ ] End-to-end, below.
- End-to-end:
  - `mg search 'NALICZENIE Kazimierza'` → `mg attachment save <msg-id> --out /tmp/x` → Claude reads the PDF with the Read tool. This closes today's loop.
  - Check the binary: `ldd target/release/mg` shows only libc and libgcc_s (plus libdbus if not vendored).
  - M3: `mg reply <id> --body test` creates a draft visible in Gmail. Piping `mg draft send <id>` with no TTY is refused. Running it in a real terminal shows the confirmation prompt.
