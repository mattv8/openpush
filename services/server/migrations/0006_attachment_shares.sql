ALTER TABLE upload_reservations ADD COLUMN deleting_at TIMESTAMPTZ;
CREATE INDEX upload_reservations_expiry_cleanup ON upload_reservations(vault_id, expires_at) WHERE finalized_at IS NULL AND deleting_at IS NULL;

CREATE TABLE public_attachment_copies (
  share_id UUID PRIMARY KEY,
  vault_id UUID NOT NULL REFERENCES vaults(vault_id),
  attachment_id UUID NOT NULL,
  object_key TEXT NOT NULL UNIQUE,
  token_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(token_digest) = 32),
  safe_name TEXT NOT NULL,
  media_type TEXT NOT NULL,
  byte_count BIGINT NOT NULL CHECK (byte_count > 0 AND byte_count <= 10485760),
  created_by_device_id UUID NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at TIMESTAMPTZ NOT NULL DEFAULT now() + interval '7 days',
  revoked_at TIMESTAMPTZ,
  FOREIGN KEY (vault_id, attachment_id) REFERENCES attachments(vault_id, attachment_id),
  FOREIGN KEY (vault_id, created_by_device_id) REFERENCES devices(vault_id, device_id)
);
CREATE INDEX public_attachment_copies_token_lookup ON public_attachment_copies(token_digest) WHERE revoked_at IS NULL;
