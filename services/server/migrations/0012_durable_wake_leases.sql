-- Wake work is durable across server replicas.  The payload identifiers are
-- random, persisted capabilities rather than transport or vault identifiers.
ALTER TABLE device_wake_routes
  ADD COLUMN generation BIGINT NOT NULL DEFAULT 1;

ALTER TABLE device_wake_jobs
  ADD COLUMN job_id UUID,
  ADD COLUMN opaque_nonce TEXT,
  ADD COLUMN lease_token UUID,
  ADD COLUMN lease_until TIMESTAMPTZ,
  ADD COLUMN last_attempt_at TIMESTAMPTZ;

UPDATE device_wake_jobs
  SET job_id = gen_random_uuid(),
      opaque_nonce = encode(uuid_send(gen_random_uuid()) || uuid_send(gen_random_uuid()), 'base64')
  WHERE job_id IS NULL OR opaque_nonce IS NULL;

ALTER TABLE device_wake_jobs
  ALTER COLUMN job_id SET NOT NULL,
  ALTER COLUMN opaque_nonce SET NOT NULL;

CREATE UNIQUE INDEX device_wake_jobs_job_id ON device_wake_jobs(job_id);
CREATE INDEX device_wake_jobs_lease_due ON device_wake_jobs(available_at, lease_until);
