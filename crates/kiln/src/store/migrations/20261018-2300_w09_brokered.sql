-- Brokered secrets (docs/ci.md W09-R08): where a secret comes from instead of a stored value
-- (`integration:aws-prod`, `aws-sm:arn:…`, …). Its value is made at leg start, never stored.
ALTER TABLE ci_secrets ADD COLUMN broker TEXT;
