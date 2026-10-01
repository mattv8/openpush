# OpenPush development boundaries

- Work in the current checkout and preserve unrelated changes. Do not stage, commit, publish, or deploy without the user's authorization.
- Rust owns domain/protocol state and the client SQLCipher database. Native hosts own network connections, OS scheduling, credential storage, cancellation, and carrier APIs. React renders sanitized view models; it must never receive passphrases, device credentials, database keys, or encryption keys.
- Device synchronization uses a manually shared passphrase. Do not replace this with automatic key distribution or claim forward secrecy. Carrier SMS/MMS is outside the application's encryption. Published attachment copies are a separate, explicit plaintext sharing action.
- Keep transport acknowledgment, local durable receipt, application state, carrier submission, and delivery evidence separate. Never automatically resend a command whose carrier outcome is uncertain.
- Shared wire contracts come from Rust. Generate bindings/contracts reproducibly; do not hand-maintain equivalent protocol models across platforms.
- Android is companion-first. Do not request the default-SMS role, use hidden APIs, or claim Android RCS access without a verified integration. iOS carrier support is capability/region/default-app gated; signing does not establish eligibility.
- Keep simulator, real-carrier, store-distributable, and production-readiness evidence separate. Fixtures and experimental window probes must remain visibly labeled.
- Native floating heads require real input-region/focus verification. CSS rounding and view hit-test math do not prove transparent-window click-through. Preserve usable main-window/composer fallbacks.
- Run focused checks with real persistence for durability/security behavior. Mock only genuine external boundaries, never a duplicate implementation of the code under test. Report blocked or unexecuted checks honestly.
- Keep `.opencode/`, `.openchamber/`, and `docs/` scratch untracked. Retain only reusable product/tooling/regression code; do not add single-use bootstrap scripts or generated build binaries to source.
