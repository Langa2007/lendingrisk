ALTER TABLE position
  ADD COLUMN IF NOT EXISTS stable_debt numeric NOT NULL DEFAULT 0;

ALTER TABLE position_history
  ADD COLUMN IF NOT EXISTS stable_debt numeric NOT NULL DEFAULT 0;
