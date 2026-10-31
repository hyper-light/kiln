-- kiln:epoch 5
-- HELD U05: authority is registered by the authenticated original ordinary claim.
-- No old log frame, mutable file, live End or physical Stopped creates this binding.
CREATE TABLE ci_log_producers (
    binding TEXT PRIMARY KEY CHECK(length(binding)=64),
    descriptor BLOB NOT NULL CHECK(length(descriptor)<=8192),
    job TEXT NOT NULL, job_order INTEGER NOT NULL, attempt INTEGER NOT NULL,
    agent TEXT NOT NULL, view TEXT NOT NULL,
    capture_limit INTEGER NOT NULL CHECK(capture_limit>=0),
    through TEXT NOT NULL CHECK(length(through)=16),
    bytes INTEGER NOT NULL DEFAULT 0 CHECK(bytes>=0),
    steps INTEGER NOT NULL DEFAULT 0 CHECK(steps>=0 AND steps<=4096),
    root TEXT NOT NULL CHECK(length(root)=64),
    state TEXT NOT NULL DEFAULT 'open' CHECK(state IN ('open','closed','disposed')),
    active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1)),
    received INTEGER NOT NULL DEFAULT 0 CHECK(received IN (0,1)),
    end_ready INTEGER NOT NULL DEFAULT 0 CHECK(end_ready IN (0,1)),
    disposition TEXT CHECK(disposition IS NULL OR disposition='no_start'),
    result BLOB CHECK(result IS NULL OR length(result)<=65536),
    result_hash TEXT CHECK(result_hash IS NULL OR length(result_hash)=64),
    close_operation TEXT
);
CREATE UNIQUE INDEX ci_log_one_active_producer ON ci_log_producers(job,job_order,attempt) WHERE active=1;
CREATE INDEX ci_log_producers_lookup ON ci_log_producers(job,attempt,agent);
CREATE INDEX ci_log_producers_view ON ci_log_producers(view,binding);
CREATE INDEX ci_log_producers_open ON ci_log_producers(agent,binding) WHERE state='open';
CREATE INDEX ci_log_producers_recover ON ci_log_producers(binding) WHERE end_ready=1;

-- Full u64 item numbers are fixed-width lowercase hexadecimal, not narrowed SQL i64.
-- Bytes belong to the configured immutable Blobs input; only bounded identity is SQL.
CREATE TABLE ci_log_producer_items (
    binding TEXT NOT NULL, item TEXT NOT NULL CHECK(length(item)=16),
    operation TEXT NOT NULL UNIQUE CHECK(length(operation)=64),
    kind TEXT NOT NULL CHECK(kind IN ('chunk','end')),
    body_hash TEXT NOT NULL CHECK(length(body_hash)=64),
    body_bytes INTEGER NOT NULL CHECK(body_bytes>0 AND body_bytes<=98304),
    step INTEGER NOT NULL CHECK(step>=0 AND step<=4294967295),
    offset INTEGER NOT NULL CHECK(offset>=0),
    previous_root TEXT NOT NULL CHECK(length(previous_root)=64),
    root TEXT NOT NULL CHECK(length(root)=64),
    input_hash TEXT NOT NULL CHECK(length(input_hash)=64),
    input_bytes INTEGER NOT NULL CHECK(input_bytes>=0 AND input_bytes<=32768),
    end_body BLOB CHECK(end_body IS NULL OR length(end_body)<=98304),
    final_seq TEXT CHECK(final_seq IS NULL OR length(final_seq)=16),
    captured_bytes INTEGER CHECK(captured_bytes IS NULL OR captured_bytes>=0),
    result BLOB CHECK(result IS NULL OR length(result)<=65536),
    result_hash TEXT CHECK(result_hash IS NULL OR length(result_hash)=64),
    capture BLOB CHECK(capture IS NULL OR length(capture)<=8192),
    receipt BLOB CHECK(receipt IS NULL OR length(receipt)<=8192),
    refusal TEXT CHECK(refusal IS NULL OR length(CAST(refusal AS BLOB))<=4096),
    PRIMARY KEY(binding,item)
);
CREATE UNIQUE INDEX ci_log_one_producer_end ON ci_log_producer_items(binding) WHERE kind='end';
CREATE TABLE ci_log_producer_steps (
    binding TEXT NOT NULL, step INTEGER NOT NULL,
    bytes INTEGER NOT NULL CHECK(bytes>=0), PRIMARY KEY(binding,step)
);
-- Sticky exact-attempt NoStart reuse disposition. One indexed check avoids scanning a
-- growing history of positively disposed empty grants inside each new claim.
CREATE TABLE ci_log_producer_reuse (
    job TEXT NOT NULL, job_order INTEGER NOT NULL, attempt INTEGER NOT NULL,
    view TEXT NOT NULL, fenced INTEGER NOT NULL CHECK(fenced IN (0,1)),
    PRIMARY KEY(job,job_order,attempt)
);
CREATE INDEX ci_log_producer_reuse_view ON ci_log_producer_reuse(view,attempt);
