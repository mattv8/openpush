CREATE TABLE relay_registrations (
    id UUID PRIMARY KEY,
    provider TEXT NOT NULL CHECK (provider IN ('apns', 'fcm')),
    token TEXT NOT NULL,
    challenge_digest BYTEA NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    confirmed_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    installation_public_identity TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE relay_routes (
    id UUID PRIMARY KEY,
    registration_id UUID NOT NULL UNIQUE REFERENCES relay_registrations(id),
    provider TEXT NOT NULL,
    token TEXT NOT NULL,
    manage_digest BYTEA NOT NULL,
    wake_digest BYTEA NOT NULL,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE relay_wake_jobs (
    route_id UUID PRIMARY KEY REFERENCES relay_routes(id) ON DELETE CASCADE,
    idempotency_id TEXT NOT NULL,
    opaque_nonce TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'attempted', 'failed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX relay_registrations_expiry_idx ON relay_registrations (expires_at);
