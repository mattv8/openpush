-- Durable, retryable object deletion and fenced public-copy lifecycle.
--
-- Every object key that may become garbage is recorded here before or in the
-- same transaction that makes it unreferenced. Upload attempts are recorded as
-- future-dated intents before their PUT and are claimed (row deleted) when the
-- attempt becomes the reservation's object. The maintenance worker deletes due
-- keys that nothing references and retries failures with backoff.
CREATE TABLE storage_deletions (
  object_key TEXT PRIMARY KEY,
  vault_id UUID NOT NULL,
  reason TEXT NOT NULL CHECK (reason IN ('upload_attempt', 'superseded_upload', 'expired_reservation', 'public_copy')),
  not_before TIMESTAMPTZ NOT NULL DEFAULT now(),
  attempts INTEGER NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX storage_deletions_due ON storage_deletions(not_before);

-- Legacy fenced reservations whose single inline delete attempt failed are
-- handed to the retrying queue instead of lingering forever.
INSERT INTO storage_deletions(object_key, vault_id, reason)
SELECT object_key, vault_id, 'expired_reservation'
FROM upload_reservations
WHERE deleting_at IS NOT NULL AND finalized_at IS NULL
ON CONFLICT (object_key) DO NOTHING;
DELETE FROM upload_reservations WHERE deleting_at IS NOT NULL AND finalized_at IS NULL;

CREATE INDEX upload_reservations_expired ON upload_reservations(expires_at)
  WHERE finalized_at IS NULL AND deleting_at IS NULL;

-- Public copies: ready only after their object exists; retired when revoked or
-- expired (queued for deletion); purged once the object is gone. Quota counts
-- every copy that is not purged.
ALTER TABLE public_attachment_copies
  ADD COLUMN ready_at TIMESTAMPTZ,
  ADD COLUMN retired_at TIMESTAMPTZ,
  ADD COLUMN purged_at TIMESTAMPTZ;
UPDATE public_attachment_copies SET ready_at = created_at;
UPDATE public_attachment_copies
SET retired_at = COALESCE(revoked_at, now())
WHERE revoked_at IS NOT NULL OR expires_at <= now();
INSERT INTO storage_deletions(object_key, vault_id, reason)
SELECT object_key, vault_id, 'public_copy'
FROM public_attachment_copies
WHERE retired_at IS NOT NULL
ON CONFLICT (object_key) DO NOTHING;
CREATE INDEX public_attachment_copies_expiry ON public_attachment_copies(expires_at)
  WHERE retired_at IS NULL;
CREATE INDEX public_attachment_copies_quota ON public_attachment_copies(vault_id)
  WHERE purged_at IS NULL;

-- Replay pruning scans for any vault with expired rows by created_at.
CREATE INDEX event_log_created_at ON event_log(created_at);
