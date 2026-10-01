CREATE TABLE vaults (
  vault_id UUID PRIMARY KEY,
  public_key_profile JSONB NOT NULL,
  encrypted_vault_check_header BYTEA NOT NULL,
  key_epoch INTEGER NOT NULL CHECK (key_epoch > 0),
  profile_fingerprint CHAR(64) NOT NULL,
  next_cursor BIGINT NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE devices (
  vault_id UUID NOT NULL REFERENCES vaults(vault_id),
  device_id UUID NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('owner','device','gateway')),
  public_key JSONB NOT NULL,
  profile_fingerprint CHAR(64) NOT NULL,
  key_epoch INTEGER NOT NULL CHECK (key_epoch > 0),
  revoked_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (vault_id, device_id)
);
CREATE TABLE device_credentials (
  token_digest BYTEA PRIMARY KEY,
  vault_id UUID NOT NULL,
  device_id UUID NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  revoked_at TIMESTAMPTZ,
  FOREIGN KEY (vault_id, device_id) REFERENCES devices(vault_id, device_id)
);
CREATE TABLE pairing_challenges (
  challenge_digest BYTEA PRIMARY KEY,
  vault_id UUID NOT NULL REFERENCES vaults(vault_id),
  requested_device_id UUID NOT NULL,
  requested_public_key JSONB NOT NULL,
  approved_by_device_id UUID NOT NULL,
  profile_fingerprint CHAR(64) NOT NULL,
  key_epoch INTEGER NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL,
  consumed_at TIMESTAMPTZ
);
CREATE TABLE event_log (
  vault_id UUID NOT NULL,
  cursor BIGINT NOT NULL CHECK (cursor > 0),
  envelope_id UUID NOT NULL,
  producer_device_id UUID NOT NULL,
  producer_sequence BIGINT NOT NULL CHECK (producer_sequence > 0),
  purpose TEXT NOT NULL CHECK (purpose IN ('event','command')),
  command_id UUID,
  cipher_digest BYTEA NOT NULL,
  envelope JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (vault_id, cursor),
  UNIQUE (vault_id, producer_device_id, producer_sequence),
  UNIQUE (vault_id, producer_device_id, envelope_id)
);
CREATE UNIQUE INDEX event_log_command_id_unique
  ON event_log(vault_id, producer_device_id, command_id)
  WHERE command_id IS NOT NULL;
CREATE TABLE encrypted_records (vault_id UUID NOT NULL, envelope_id UUID NOT NULL, envelope JSONB NOT NULL, PRIMARY KEY(vault_id,envelope_id));
CREATE TABLE commands (vault_id UUID NOT NULL, producer_device_id UUID NOT NULL, command_id UUID NOT NULL, gateway_device_id UUID NOT NULL, cipher_digest BYTEA NOT NULL, cursor BIGINT NOT NULL, PRIMARY KEY(vault_id,producer_device_id,command_id));
CREATE TABLE command_receipts (vault_id UUID NOT NULL, command_id UUID NOT NULL, gateway_device_id UUID NOT NULL, receipt JSONB NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY(vault_id,command_id,gateway_device_id));
CREATE TABLE device_cursors (vault_id UUID NOT NULL, device_id UUID NOT NULL, receive_cursor BIGINT NOT NULL DEFAULT 0, PRIMARY KEY(vault_id,device_id));
CREATE TABLE outbox_jobs (vault_id UUID NOT NULL, cursor BIGINT NOT NULL, kind TEXT NOT NULL, payload JSONB NOT NULL, available_at TIMESTAMPTZ NOT NULL DEFAULT now(), attempts INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(vault_id,cursor,kind));
