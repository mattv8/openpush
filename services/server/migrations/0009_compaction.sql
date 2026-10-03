-- Authenticated client compaction metadata is indexed separately so immutable
-- ciphertext rows remain immutable. Unmarked legacy rows are deliberately not
-- candidates for deletion.
ALTER TABLE vaults ADD COLUMN compaction_generation BIGINT NOT NULL DEFAULT 0;
ALTER TABLE vaults ADD COLUMN last_compacted_at TIMESTAMPTZ;
ALTER TABLE devices ADD COLUMN compaction_generation_fence BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX devices_compaction_fence ON devices(vault_id) WHERE revoked_at IS NULL;

-- Immutable retry identity survives physical ciphertext removal.
CREATE TABLE compacted_records (
  vault_id UUID NOT NULL,
  producer_device_id UUID NOT NULL,
  producer_sequence BIGINT NOT NULL CHECK (producer_sequence > 0),
  envelope_id UUID NOT NULL,
  command_id UUID,
  cipher_digest BYTEA NOT NULL,
  original_cursor BIGINT NOT NULL,
  compacted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (vault_id, producer_device_id, producer_sequence),
  UNIQUE (vault_id, producer_device_id, envelope_id)
);

-- Ciphertext remains opaque: native hosts explicitly register every attachment
-- reference before asking the server to reclaim the object.
CREATE TABLE attachment_record_references (
  vault_id UUID NOT NULL,
  attachment_id UUID NOT NULL,
  producer_device_id UUID NOT NULL,
  producer_sequence BIGINT NOT NULL CHECK (producer_sequence > 0),
  registered_by_device_id UUID NOT NULL,
  PRIMARY KEY (vault_id, attachment_id, producer_device_id, producer_sequence)
);

CREATE TABLE record_compaction (
  vault_id UUID NOT NULL,
  cursor BIGINT NOT NULL,
  compaction_key BYTEA NOT NULL CHECK (octet_length(compaction_key) = 32),
  terminal BOOLEAN NOT NULL DEFAULT FALSE,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (vault_id, cursor),
  FOREIGN KEY (vault_id, cursor) REFERENCES encrypted_records(vault_id, cursor)
);
CREATE INDEX record_compaction_key ON record_compaction(vault_id, compaction_key);

CREATE TABLE record_supersessions (
  vault_id UUID NOT NULL,
  by_cursor BIGINT NOT NULL,
  target_producer_device_id UUID NOT NULL,
  target_producer_sequence BIGINT NOT NULL CHECK (target_producer_sequence > 0),
  PRIMARY KEY (vault_id, by_cursor, target_producer_device_id, target_producer_sequence),
  FOREIGN KEY (vault_id, by_cursor) REFERENCES encrypted_records(vault_id, cursor)
);
CREATE INDEX record_supersessions_target
  ON record_supersessions(vault_id, target_producer_device_id, target_producer_sequence);

-- A finalized private object may be reclaimed only after its owner presents a
-- compaction cut that the server can verify. Public derivatives have their own
-- object keys and lifecycle, so keep their rows after the private row is gone.
ALTER TABLE public_attachment_copies
  DROP CONSTRAINT public_attachment_copies_vault_id_attachment_id_fkey;
ALTER TABLE storage_deletions DROP CONSTRAINT storage_deletions_reason_check;
ALTER TABLE storage_deletions ADD CONSTRAINT storage_deletions_reason_check
  CHECK (reason IN ('upload_attempt', 'superseded_upload', 'expired_reservation', 'public_copy', 'finalized_attachment'));

-- The immutable log permits DELETE only while the compaction transaction sets
-- its transaction-local guard. UPDATE remains forbidden unconditionally.
CREATE OR REPLACE FUNCTION peppy_reject_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' AND current_setting('peppy.compaction', true) = 'on' THEN
    RETURN OLD;
  END IF;
  RAISE EXCEPTION '% rows are immutable', TG_TABLE_NAME
    USING ERRCODE = 'integrity_constraint_violation';
END
$$;
