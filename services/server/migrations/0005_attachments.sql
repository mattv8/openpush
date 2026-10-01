CREATE TABLE upload_reservations (
  vault_id UUID NOT NULL REFERENCES vaults(vault_id),
  attachment_id UUID NOT NULL,
  device_id UUID NOT NULL,
  object_key TEXT NOT NULL UNIQUE,
  declared_bytes BIGINT NOT NULL CHECK (declared_bytes > 0 AND declared_bytes <= 67108864),
  declared_sha256 BYTEA NOT NULL CHECK (octet_length(declared_sha256) = 32),
  uploaded_bytes BIGINT,
  uploaded_sha256 BYTEA,
  uploaded_at TIMESTAMPTZ,
  finalized_at TIMESTAMPTZ,
  expires_at TIMESTAMPTZ NOT NULL DEFAULT now() + interval '1 hour',
  PRIMARY KEY (vault_id, attachment_id),
  FOREIGN KEY (vault_id, device_id) REFERENCES devices(vault_id, device_id)
);
CREATE TABLE attachments (
  vault_id UUID NOT NULL REFERENCES vaults(vault_id),
  attachment_id UUID NOT NULL,
  object_key TEXT NOT NULL UNIQUE,
  ciphertext_bytes BIGINT NOT NULL,
  ciphertext_sha256 BYTEA NOT NULL CHECK (octet_length(ciphertext_sha256) = 32),
  created_by_device_id UUID NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (vault_id, attachment_id)
);
