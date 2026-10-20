-- The conversation workload's events, as kiln's store keeps them (crates/kiln/src/convo/sql.rs),
-- sharded by session (vschema.json).
CREATE TABLE events (
  session VARCHAR(64) NOT NULL,
  id VARCHAR(64) NOT NULL,
  seq BIGINT NOT NULL,
  at BIGINT NOT NULL,
  kind VARCHAR(16) NOT NULL,
  text TEXT NOT NULL,
  body MEDIUMTEXT NOT NULL,
  PRIMARY KEY (session, id),
  KEY by_seq (session, seq),
  KEY by_time (at)
) ENGINE = InnoDB;
