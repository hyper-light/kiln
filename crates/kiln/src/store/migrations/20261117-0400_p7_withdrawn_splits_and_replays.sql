-- The deployment-experiment split of `/api/experiments` (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1, 853543f7e)
-- and its replays (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.1b, a9fbded98) are withdrawn: an agent definition's
-- experiments (`crate::agentdefs::experiments`, the Agents page's model) split its sessions, and "Replays weren't and
-- never were a part of experiments" (the person, 2026-11-16). What they kept in `kv` goes with them, once: each split
-- (an experiment whose record names a `target`) with its decision and cancellation, and every split's pause, draws,
-- replays and replays' runs. Experiments of options keep theirs. Keys are taken by range (a prefix and the prefix with
-- its last `/` made `0`, the next character), so no `%` or `_` in an id reads as a pattern.
DELETE FROM kv WHERE k IN (
    SELECT 'ci/experiment-decision/' || substr(k, 15) FROM kv
    WHERE k >= 'ci/experiment/' AND k < 'ci/experiment0' AND json_valid(v) AND json_extract(v, '$.target') IS NOT NULL
    UNION
    SELECT 'ci/experiment-cancel/' || substr(k, 15) FROM kv
    WHERE k >= 'ci/experiment/' AND k < 'ci/experiment0' AND json_valid(v) AND json_extract(v, '$.target') IS NOT NULL
);
DELETE FROM kv WHERE k >= 'ci/experiment/' AND k < 'ci/experiment0' AND json_valid(v) AND json_extract(v, '$.target') IS NOT NULL;
DELETE FROM kv WHERE (k >= 'ci/experiment-paused/' AND k < 'ci/experiment-paused0')
    OR (k >= 'ci/experiment-drawn/' AND k < 'ci/experiment-drawn0')
    OR (k >= 'ci/experiment-replay/' AND k < 'ci/experiment-replay0')
    OR (k >= 'ci/experiment-replayed/' AND k < 'ci/experiment-replayed0');
