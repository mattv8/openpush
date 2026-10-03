# Optional push relay

The relay is a separately deployable, optional service. It stores only provider tokens, route relationships, credential digests, bounded wake jobs, and limited operational timing/IP metadata. It never receives message bodies, contact identifiers, attachment URLs, or user-server URLs.

`PEPPY_RELAY_PROVIDER=unconfigured` (the only currently supported production setting) makes registration and wake requests return `503 provider_unconfigured`. This is intentional: APNs/FCM credentials and authenticated provider adapters are not supplied by this repository. Do not interpret a healthy relay as live push delivery.

Provide `DATABASE_URL` and optionally `BIND_ADDR` (default `127.0.0.1:8090`). The relay keeps only an in-memory, one-minute IP admission window; it does not currently claim configurable metadata retention. Deploy behind TLS. Forwarded headers are deliberately not trusted: admission uses the connected peer IP. Keep provider credentials in the deployment secret manager if a reviewed fixed-origin provider adapter is added later; never place them in this repository or in an application server request.
