CREATE TABLE relay_wake_deliveries (
    route_id UUID NOT NULL REFERENCES relay_routes(id) ON DELETE CASCADE,
    idempotency_id TEXT NOT NULL,
    delivered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (route_id, idempotency_id)
);
CREATE INDEX relay_wake_deliveries_retention_idx ON relay_wake_deliveries (delivered_at);
