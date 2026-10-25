-- A fleet change's proposals in apps' source repositories (docs/deploy-api.md §3.3): one for each
-- repository, a pull request on its host or a branch on kiln's own git server, followed together.
CREATE TABLE IF NOT EXISTS deploy_fleet_proposals (
    fleet TEXT NOT NULL,
    repo TEXT NOT NULL,
    spec TEXT NOT NULL,
    PRIMARY KEY (fleet, repo)
);
