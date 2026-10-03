-- A4: immutable ciphertext snapshot records, bounded replay retention,
-- lightweight outbox references and immutable per-epoch key profiles.
-- This migration upgrades earlier local 0001-0006 schemas in place. It never
-- deletes vault data; it fails rather than guessing if a legacy record has no
-- matching committed event identity.

-- 1. Immutable records keyed by the protocol's producer-scoped identity.
ALTER TABLE encrypted_records
  ADD COLUMN producer_device_id UUID,
  ADD COLUMN cursor BIGINT,
  ADD COLUMN producer_sequence BIGINT,
  ADD COLUMN purpose TEXT,
  ADD COLUMN command_id UUID,
  ADD COLUMN cipher_digest BYTEA,
  ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();

UPDATE encrypted_records r
SET producer_device_id = e.producer_device_id,
    cursor = e.cursor,
    producer_sequence = e.producer_sequence,
    purpose = e.purpose,
    command_id = e.command_id,
    cipher_digest = e.cipher_digest,
    created_at = e.created_at
FROM event_log e
WHERE e.vault_id = r.vault_id
  AND e.envelope_id = r.envelope_id
  AND e.producer_device_id::text = r.envelope->>'producer_device_id';

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM encrypted_records WHERE cursor IS NULL) THEN
    RAISE EXCEPTION 'encrypted_records contains rows without a committed event identity; refusing to rekey or drop them';
  END IF;
END
$$;

ALTER TABLE encrypted_records
  ALTER COLUMN producer_device_id SET NOT NULL,
  ALTER COLUMN cursor SET NOT NULL,
  ALTER COLUMN producer_sequence SET NOT NULL,
  ALTER COLUMN purpose SET NOT NULL,
  ALTER COLUMN cipher_digest SET NOT NULL,
  DROP CONSTRAINT encrypted_records_pkey,
  ADD PRIMARY KEY (vault_id, producer_device_id, envelope_id),
  ADD CONSTRAINT encrypted_records_cursor_unique UNIQUE (vault_id, cursor),
  ADD CONSTRAINT encrypted_records_sequence_unique UNIQUE (vault_id, producer_device_id, producer_sequence),
  ADD CONSTRAINT encrypted_records_cursor_positive CHECK (cursor > 0),
  ADD CONSTRAINT encrypted_records_sequence_positive CHECK (producer_sequence > 0),
  ADD CONSTRAINT encrypted_records_purpose_check CHECK (purpose IN ('event', 'command')),
  ADD CONSTRAINT encrypted_records_command_shape CHECK ((purpose = 'command') = (command_id IS NOT NULL));
CREATE UNIQUE INDEX encrypted_records_command_unique
  ON encrypted_records(vault_id, producer_device_id, command_id)
  WHERE command_id IS NOT NULL;

CREATE FUNCTION peppy_reject_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION '% rows are immutable', TG_TABLE_NAME
    USING ERRCODE = 'integrity_constraint_violation';
END
$$;
CREATE TRIGGER encrypted_records_immutable
  BEFORE UPDATE OR DELETE ON encrypted_records
  FOR EACH ROW EXECUTE FUNCTION peppy_reject_mutation();

-- 2. Replay retention: rows at or below the floor have been pruned from the
-- transport log. Immutable records above remain available to snapshots.
ALTER TABLE vaults
  ADD COLUMN replay_floor_cursor BIGINT NOT NULL DEFAULT 0,
  ADD CONSTRAINT vaults_replay_floor_bounds
    CHECK (replay_floor_cursor >= 0 AND replay_floor_cursor <= next_cursor);
CREATE INDEX event_log_retention ON event_log(vault_id, created_at);

-- 3. Outbox jobs are references to committed cursors; the envelope remains in
-- event_log/encrypted_records only. Existing legacy payloads are left intact
-- and are removed with their job after delivery retention.
ALTER TABLE outbox_jobs
  ALTER COLUMN payload DROP NOT NULL,
  ADD COLUMN delivered_at TIMESTAMPTZ;
CREATE INDEX outbox_jobs_due ON outbox_jobs(available_at) WHERE delivered_at IS NULL;
CREATE INDEX outbox_jobs_delivered ON outbox_jobs(delivered_at) WHERE delivered_at IS NOT NULL;

CREATE INDEX commands_gateway_pending ON commands(vault_id, gateway_device_id, cursor);

-- 4. Immutable public KDF metadata per key epoch. Only activated_at may be set
-- once; historical rows remain for manual old-passphrase recovery.
CREATE TABLE vault_key_profiles (
  vault_id UUID NOT NULL REFERENCES vaults(vault_id),
  key_epoch INTEGER NOT NULL CHECK (key_epoch > 0),
  public_key_profile JSONB NOT NULL,
  encrypted_vault_check_header BYTEA NOT NULL
    CHECK (octet_length(encrypted_vault_check_header) BETWEEN 1 AND 1048576),
  profile_fingerprint CHAR(64) NOT NULL,
  created_by_device_id UUID,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  activated_at TIMESTAMPTZ,
  PRIMARY KEY (vault_id, key_epoch),
  UNIQUE (vault_id, profile_fingerprint)
);
INSERT INTO vault_key_profiles(vault_id, key_epoch, public_key_profile, encrypted_vault_check_header, profile_fingerprint, created_at, activated_at)
SELECT vault_id, key_epoch, public_key_profile, encrypted_vault_check_header, profile_fingerprint, created_at, created_at
FROM vaults;

CREATE FUNCTION peppy_key_profile_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    RAISE EXCEPTION 'vault key profiles are immutable'
      USING ERRCODE = 'integrity_constraint_violation';
  END IF;
  IF NEW.vault_id IS DISTINCT FROM OLD.vault_id
     OR NEW.key_epoch IS DISTINCT FROM OLD.key_epoch
     OR NEW.public_key_profile IS DISTINCT FROM OLD.public_key_profile
     OR NEW.encrypted_vault_check_header IS DISTINCT FROM OLD.encrypted_vault_check_header
     OR NEW.profile_fingerprint IS DISTINCT FROM OLD.profile_fingerprint
     OR NEW.created_by_device_id IS DISTINCT FROM OLD.created_by_device_id
     OR NEW.created_at IS DISTINCT FROM OLD.created_at
     OR (OLD.activated_at IS NOT NULL AND NEW.activated_at IS DISTINCT FROM OLD.activated_at)
  THEN
    RAISE EXCEPTION 'vault key profiles are immutable'
      USING ERRCODE = 'integrity_constraint_violation';
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER vault_key_profiles_immutable
  BEFORE UPDATE OR DELETE ON vault_key_profiles
  FOR EACH ROW EXECUTE FUNCTION peppy_key_profile_guard();
