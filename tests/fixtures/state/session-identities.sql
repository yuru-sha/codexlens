CREATE TABLE threads (id TEXT, session_id TEXT, rollout_id TEXT, rollout_path TEXT, parent_thread_id TEXT, cwd TEXT);
INSERT INTO threads VALUES ('fixture-main-thread', 'fixture-session-tree', 'fixture-rollout', '/fixture/rollout-2026-01-02T00-00-00-fixture-main-thread_fixture-rollout.jsonl', NULL, '/fixture/main');
INSERT INTO threads VALUES ('fixture-child-thread', 'fixture-session-tree', 'fixture-child-rollout', '/fixture/rollout-2026-01-02T00-01-00-fixture-child-thread_fixture-child-rollout.jsonl', 'fixture-main-thread', '/fixture/child');
INSERT INTO threads VALUES ('fixture-child-two', 'fixture-session-tree', 'fixture-child-two-rollout', '/fixture/rollout-2026-01-02T00-02-00-fixture-child-two_fixture-child-two-rollout.jsonl', 'fixture-main-thread', '/fixture/child-two');
