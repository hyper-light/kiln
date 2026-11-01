-- Why kiln itself handed a change's threads to its agent, or held them for a person (docs/ci.md
-- W26-R18): a hand-off `by` kiln says why (an agent's feedback, which the repository lets agents fix
-- code from, and who turned that on and when); a row whose `what` is `held` keeps why kiln sent
-- nothing (the change's agent has ended: the fix waits for a person to start or resume it), and
-- Ship's item says so until a new revision. A person's hand-offs have none. A nullable column: an
-- older kiln never names it.
ALTER TABLE ship_handoffs ADD COLUMN why TEXT;
