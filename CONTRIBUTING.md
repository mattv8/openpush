# Contributing

OpenPush is an in-progress developer foundation. Preserve the distinction between simulator evidence, native build evidence, carrier evidence, and production/store readiness. Do not commit `.env`, build outputs, local databases, generated credentials, passphrases, SQLCipher keys, device tokens, or signing material. Use synthetic credentials only in a manually created ignored `.env`.

## Before proposing a change

Run focused checks for the code you changed. For a server or shared Rust change, start with:

```sh
just cargo-fmt
just lint
just server-test
just contracts-check
docker compose --env-file .env -f docker-compose.yml config --quiet
```

Use `just desktop-test`, `just android-test`, `just ios-test`, or `just ffi-smoke` when their corresponding surface changes. The Swift targets exercise macOS host tests and generated Rust-call smoke checks, not an iOS simulator. Android checks include its adapter regressions and lint as well as the APK. Run `just integration-test` when the change needs real persistence and Compose-backed integration coverage.

Generated contracts currently cover the envelope/domain schemas and UniFFI bindings. The OpenAPI artifact is not a complete HTTP route specification: REST request/response adapters are still maintained in the server and native clients. Changes to those routes require matching client updates and real-server integration checks; a passing `contracts-check` alone does not prove HTTP compatibility. Mobile host tests with a fake HTTP transport are also not real-server integration evidence.

Before integration, run the appropriate final workspace checks with `--locked`. Do not hand-edit `Cargo.lock`; only Cargo may resolve it. The root Cargo manifest and lockfile are shared integration files, so do not regenerate the root lockfile concurrently with another package change.

## Security and operational boundaries

- The manually shared vault passphrase stays on clients. Do not add automatic key distribution or claim forward secrecy.
- Carrier SMS/MMS is plaintext outside the application encryption boundary. Keep transport acknowledgement, local durable receipt, application state, carrier submission, and delivery evidence separate. Never automatically resend a command with an uncertain carrier outcome.
- Public attachment copies are explicit plaintext derivatives, separate from encrypted originals. Do not blur their sharing, revocation, or security model.
- Rust owns protocol/domain state and the client SQLCipher database. Native hosts own network connections, OS scheduling, credential storage, cancellation, and carrier APIs. React receives sanitized view models only—never passphrases, database keys, device credentials, or encryption keys.
- Keep simulator, real-carrier, store-distributable, and production-ready claims separate. The Android shell is companion-first and must not request default-SMS, `WRITE_SMS`, hidden APIs, or unverified RCS access. iOS carrier claims require real capability evidence.
- Snapshot history is not executable carrier work. Preserve the restore guard and reconciliation behavior; do not turn a restore into automatic carrier execution.

## Commit messages and versioning

Use Conventional Commits (`type(scope): message`) to drive automatic version bumps. While major version is 0, the rules are: `feat` and breaking changes bump minor; `fix` and `perf` bump patch; `docs`, `ci`, `build`, `refactor`, `test`, `chore`, and `style` do not bump. Mark breaking changes with `type!:` (for example `feat!:`) or a `BREAKING CHANGE:` footer; while the version is `0.x` they bump minor. Unconventional subjects, including `Merge pull request …` merge commits, bump patch.

Moving to `1.0.0` is explicit: set the **Release** workflow's `version` input. After changing `infra/release/` or the release workflows, run `just release-test`.

## Audits and documentation

Run both audits when dependencies, secrets, or release-sensitive material changes:

```sh
just audit-dependencies
just audit-secrets
```

`just audit-secrets` uses `gitleaks detect --no-git` against a source snapshot that includes untracked files. This intentionally scans current authored source while excluding ignored local credentials such as `.env`. `cargo-deny` enforces dependency source, ban, and license policy; MPL-2.0 remains allowed for the pinned UniFFI dependency.

Document verification precisely. State the command and platform used, and mark hardware, carrier, simulator, or native click-through steps as unexecuted when they were not run. Do not cite session scratch files as the only operating instructions: put reusable commands in tracked documentation or link an appropriate tracked package README.

## Backup-sensitive changes

Keep backup and restore conservative. A backup may restart only services that were originally running. Restore targets must be distinct and have no existing volumes; restore may start PostgreSQL to load the dump but must leave migrations, the API, and carrier processing stopped until an operator reconciles state. Do not add automatic post-restore execution or a destructive `down -v` recovery path.

Health routes must not disclose configuration, credentials, or dependency diagnostics.
