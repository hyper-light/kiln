-- W10-R03. Change the authorization stamp in the transaction that changes access.
CREATE TABLE permission_revision (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), revision INTEGER NOT NULL);
INSERT INTO permission_revision VALUES (1, 0);
CREATE TRIGGER permission_tokens_insert AFTER INSERT ON tokens WHEN NEW.kind IN ('pat', 'agent') BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_tokens_update AFTER UPDATE OF id, hash, kind, principal, on_behalf_of, scopes, expires_at, revoked_at ON tokens WHEN NEW.kind IN ('pat', 'agent') OR OLD.kind IN ('pat', 'agent') BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_tokens_delete AFTER DELETE ON tokens WHEN OLD.kind IN ('pat', 'agent') BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_users_insert AFTER INSERT ON users BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_users_update AFTER UPDATE ON users BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_users_delete AFTER DELETE ON users BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_grants_insert AFTER INSERT ON grants BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_grants_update AFTER UPDATE ON grants BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_grants_delete AFTER DELETE ON grants BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_roles_insert AFTER INSERT ON roles BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_roles_update AFTER UPDATE ON roles BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
CREATE TRIGGER permission_roles_delete AFTER DELETE ON roles BEGIN
  UPDATE permission_revision SET revision = revision + 1 WHERE singleton = 1;
END;
