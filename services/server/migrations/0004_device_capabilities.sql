CREATE TABLE device_capabilities (
  vault_id UUID NOT NULL,
  device_id UUID NOT NULL,
  simulator BOOLEAN NOT NULL DEFAULT FALSE,
  capabilities JSONB NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (vault_id, device_id),
  FOREIGN KEY (vault_id, device_id) REFERENCES devices(vault_id, device_id)
);
