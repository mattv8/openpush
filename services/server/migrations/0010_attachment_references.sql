-- Add reference_tracked flag to upload_reservations for new CONTACT photos using
-- reference-based deletion tracking instead of timestamp-only retention.
ALTER TABLE upload_reservations ADD COLUMN reference_tracked BOOLEAN NOT NULL DEFAULT FALSE;

-- Index for efficient cleanup of expired reference-tracked reservations.
CREATE INDEX upload_reservations_reference_tracked
  ON upload_reservations(vault_id, expires_at)
  WHERE finalized_at IS NULL AND deleting_at IS NULL AND reference_tracked = TRUE;
