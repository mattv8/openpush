-- Owner-approved QR enrollment, durable relay routes, and whole-vault cleanup.
CREATE TABLE pairing_intents (
  intent_digest BYTEA PRIMARY KEY CHECK (octet_length(intent_digest) = 32),
  vault_id UUID NOT NULL REFERENCES vaults(vault_id),
  origin TEXT NOT NULL,
  created_by_device_id UUID NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL,
  claimed_at TIMESTAMPTZ,
  claimed_device_id UUID,
  claimed_public_key JSONB,
  claimed_key_digest CHAR(64),
  claim_secret_digest BYTEA CHECK (claim_secret_digest IS NULL OR octet_length(claim_secret_digest) = 32),
  challenge_token TEXT,
  challenge_digest BYTEA CHECK (challenge_digest IS NULL OR octet_length(challenge_digest) = 32),
  requested_role TEXT CHECK (requested_role IN ('device', 'gateway')),
  approved_at TIMESTAMPTZ
);
CREATE INDEX pairing_intents_expiry ON pairing_intents(expires_at) WHERE approved_at IS NULL;

CREATE TABLE device_wake_routes (
  vault_id UUID NOT NULL,
  device_id UUID NOT NULL,
  route_id UUID NOT NULL,
  wake_credential TEXT NOT NULL,
  revoked_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (vault_id, device_id),
  FOREIGN KEY (vault_id, device_id) REFERENCES devices(vault_id, device_id)
);
CREATE TABLE device_wake_jobs (
  vault_id UUID NOT NULL,
  device_id UUID NOT NULL,
  cursor BIGINT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  available_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (vault_id, device_id),
  FOREIGN KEY (vault_id, device_id) REFERENCES devices(vault_id, device_id)
);
CREATE INDEX device_wake_jobs_due ON device_wake_jobs(available_at);

ALTER TABLE storage_deletions DROP CONSTRAINT storage_deletions_reason_check;
ALTER TABLE storage_deletions ADD CONSTRAINT storage_deletions_reason_check
  CHECK (reason IN ('upload_attempt', 'superseded_upload', 'expired_reservation', 'public_copy', 'finalized_attachment', 'vault_deleted'));

-- Vault deletion is the only lifecycle operation allowed to remove immutable
-- records/profile metadata, guarded by a transaction-local server setting.
CREATE OR REPLACE FUNCTION peppy_reject_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' AND (current_setting('peppy.compaction', true) = 'on' OR current_setting('peppy.vault_delete', true) = 'on') THEN RETURN OLD; END IF;
  RAISE EXCEPTION '% rows are immutable', TG_TABLE_NAME USING ERRCODE = 'integrity_constraint_violation';
END $$;
CREATE OR REPLACE FUNCTION peppy_key_profile_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' AND current_setting('peppy.vault_delete', true) = 'on' THEN RETURN OLD; END IF;
  IF TG_OP = 'DELETE' THEN RAISE EXCEPTION 'vault key profiles are immutable' USING ERRCODE = 'integrity_constraint_violation'; END IF;
  IF NEW.vault_id IS DISTINCT FROM OLD.vault_id OR NEW.key_epoch IS DISTINCT FROM OLD.key_epoch OR NEW.public_key_profile IS DISTINCT FROM OLD.public_key_profile OR NEW.encrypted_vault_check_header IS DISTINCT FROM OLD.encrypted_vault_check_header OR NEW.profile_fingerprint IS DISTINCT FROM OLD.profile_fingerprint OR NEW.created_by_device_id IS DISTINCT FROM OLD.created_by_device_id OR NEW.created_at IS DISTINCT FROM OLD.created_at OR (OLD.activated_at IS NOT NULL AND NEW.activated_at IS DISTINCT FROM OLD.activated_at) THEN RAISE EXCEPTION 'vault key profiles are immutable' USING ERRCODE = 'integrity_constraint_violation'; END IF;
  RETURN NEW;
END $$;
