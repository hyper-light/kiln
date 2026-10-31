-- U01/W05-R28: preserve the exact accepted ordinary stop/no-start proof on its
-- existing attempt after the transient capacity journal is pruned. Missing old
-- proof remains unsupported; this optional additive field grants no new owner.
ALTER TABLE ci_attempts ADD COLUMN physical_receipt TEXT;
