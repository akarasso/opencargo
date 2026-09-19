-- The default is what makes the two-binary window safe: a binary that predates
-- scopes writes no scope, and the row it creates is still a readable `inherit`.
ALTER TABLE api_tokens ADD COLUMN scope TEXT NOT NULL DEFAULT '{"kind":"inherit"}';

-- Conservative: the routes start asking for `delete`, so everyone who could
-- remove through `write` keeps removing.
UPDATE user_permissions SET can_delete = 1 WHERE can_write = 1;
