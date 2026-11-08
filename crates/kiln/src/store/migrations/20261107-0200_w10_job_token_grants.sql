-- W10-R03 (docs/ci.md, docs/needs.md §8): a job token (GITHUB_TOKEN) is its leg's attempt's, under
-- that attempt's grant: its attempt and the grant's fence, so it's taken only while that grant is
-- its leg's live one, and revoked as the grant ends. Nullable: a job token an older build minted
-- names none, and lives as before.
ALTER TABLE tokens ADD COLUMN attempt INTEGER;
ALTER TABLE tokens ADD COLUMN grant_epoch INTEGER;
ALTER TABLE tokens ADD COLUMN grant_n INTEGER;
