# Optional push relay

The relay is a separately deployable, optional service. It stores only provider tokens, route relationships, credential digests, bounded wake jobs, and limited operational timing/IP metadata. It never receives message bodies, contact identifiers, attachment URLs, or user-server URLs.

`PEPPY_RELAY_PROVIDER=unconfigured` is the default and makes registration and wake requests return `503 provider_unconfigured`. It is safe for tests and development: no provider HTTP request is made. Set it to `apns`, `fcm`, or `apns,fcm` only in an operator deployment with secrets supplied through the environment or secret files (the corresponding `*_FILE` variable contains a file path).

APNs requires `PEPPY_RELAY_APNS_TEAM_ID`, `PEPPY_RELAY_APNS_KEY_ID`, `PEPPY_RELAY_APNS_BUNDLE_ID`, and `PEPPY_RELAY_APNS_PRIVATE_KEY` (the `.p8` content); set `PEPPY_RELAY_APNS_ENV=sandbox` for Apple's fixed sandbox origin, otherwise the fixed production origin is used. FCM requires `PEPPY_RELAY_FCM_SERVICE_ACCOUNT` (service-account JSON). Each accepts a `_FILE` alternative. Secrets are never logged. The relay uses fixed Apple and Google origins, a 10-second HTTP timeout, and sends only a bounded opaque challenge or wake nonce; it never accepts provider URLs from registrations.

Accepted wakes are durably coalesced one per route and return `202` before provider delivery. A maintenance worker retries transient failures with bounded exponential backoff. Provider invalid-token responses revoke the local route and delete its pending wake job. A configured provider is reported accurately by `/healthz` and `/readyz`; neither endpoint promises delivery.

Provide `DATABASE_URL` and optionally `BIND_ADDR` (default `127.0.0.1:8090`). The relay keeps only an in-memory, one-minute IP admission window for unauthenticated registration and confirmation; route wake and revoke requests authenticate their route-bound high-entropy credentials separately. Deploy behind TLS. Forwarded headers are deliberately not trusted: admission uses the connected peer IP. Keep provider credentials in the deployment secret manager; never place them in this repository or in an application server request.
