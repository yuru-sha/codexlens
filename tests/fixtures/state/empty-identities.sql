CREATE TABLE threads (thread_id TEXT, session_id TEXT, rollout_id TEXT, rollout_path TEXT, parent_thread_id TEXT, cwd TEXT);
INSERT INTO threads VALUES ('', 'fixture-session-tree', '', '/fixture/rollout-2026-01-02T00-00-00-fixture-main-thread_fixture-rollout.jsonl', '', '/fixture/main');
