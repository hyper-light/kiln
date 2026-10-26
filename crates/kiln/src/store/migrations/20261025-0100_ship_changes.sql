-- Ship's read model keeps every commit's standing in memory, its JSON ready, so a request reads
-- memory only. These triggers note which commits changed (sha) or which repositories' environments
-- and releases did (sha NULL), so a refresh reloads just those (plan 01a175b3a21174bc8769f5e784b90ecb).
CREATE TABLE ship_changes (seq INTEGER PRIMARY KEY AUTOINCREMENT, repo_id TEXT NOT NULL, sha TEXT);
CREATE INDEX runs_repo_sha ON runs (repo_id, sha);
CREATE TRIGGER ship_runs_insert AFTER INSERT ON runs BEGIN INSERT INTO ship_changes (repo_id, sha) VALUES (NEW.repo_id, NEW.sha); END;
CREATE TRIGGER ship_runs_update AFTER UPDATE ON runs BEGIN INSERT INTO ship_changes (repo_id, sha) VALUES (NEW.repo_id, NEW.sha); END;
CREATE TRIGGER ship_jobs_insert AFTER INSERT ON jobs BEGIN INSERT INTO ship_changes (repo_id, sha) SELECT repo_id, sha FROM runs WHERE id = NEW.run_id; END;
CREATE TRIGGER ship_jobs_update AFTER UPDATE ON jobs BEGIN INSERT INTO ship_changes (repo_id, sha) SELECT repo_id, sha FROM runs WHERE id = NEW.run_id; END;
CREATE TRIGGER ship_attempts_insert AFTER INSERT ON ci_attempts BEGIN INSERT INTO ship_changes (repo_id, sha) SELECT r.repo_id, r.sha FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.id = NEW.leg_id; END;
CREATE TRIGGER ship_attempts_update AFTER UPDATE ON ci_attempts BEGIN INSERT INTO ship_changes (repo_id, sha) SELECT r.repo_id, r.sha FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.id = NEW.leg_id; END;
CREATE TRIGGER ship_images_insert AFTER INSERT ON ci_images BEGIN INSERT INTO ship_changes (repo_id, sha) SELECT repo_id, sha FROM runs WHERE id = NEW.run_id; END;
CREATE TRIGGER ship_land_insert AFTER INSERT ON land_queue WHEN NEW.candidate IS NOT NULL BEGIN INSERT INTO ship_changes (repo_id, sha) VALUES (NEW.repo_id, NEW.candidate); END;
CREATE TRIGGER ship_land_update AFTER UPDATE ON land_queue WHEN NEW.candidate IS NOT NULL BEGIN INSERT INTO ship_changes (repo_id, sha) VALUES (NEW.repo_id, NEW.candidate); END;
CREATE TRIGGER ship_deploy_status AFTER INSERT ON ci_deployment_statuses BEGIN INSERT INTO ship_changes (repo_id, sha) SELECT repo_id, NULL FROM ci_deployments WHERE id = NEW.deployment_id; END;
CREATE TRIGGER ship_releases_insert AFTER INSERT ON deploy_releases BEGIN INSERT INTO ship_changes (repo_id, sha) SELECT id, NULL FROM repos; END;
CREATE TRIGGER ship_releases_update AFTER UPDATE ON deploy_releases BEGIN INSERT INTO ship_changes (repo_id, sha) SELECT id, NULL FROM repos; END;
CREATE TRIGGER ship_tags_insert AFTER INSERT ON ci_releases BEGIN INSERT INTO ship_changes (repo_id, sha) VALUES (NEW.repo_id, NEW.target_commitish); END;
