ALTER TABLE pairing_challenges
  ADD COLUMN requested_role TEXT NOT NULL DEFAULT 'device'
    CHECK (requested_role IN ('device', 'gateway'));
