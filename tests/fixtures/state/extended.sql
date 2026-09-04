CREATE TABLE threads (id TEXT, rollout_path TEXT, created_at TEXT, updated_at TEXT, cwd TEXT, project_path TEXT, model TEXT, model_provider TEXT, archived INTEGER, extra TEXT);
INSERT INTO threads VALUES ('fixture-extended-session', '/fixture.jsonl', 'created', 'updated', '/fixture', '/project', 'model', 'provider', 1, 'ignored');
