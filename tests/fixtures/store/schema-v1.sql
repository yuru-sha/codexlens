CREATE TABLE schema_versions (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT '');
INSERT INTO schema_versions (version) VALUES (1);
CREATE TABLE messages (message_key TEXT PRIMARY KEY, source_identity TEXT NOT NULL, source_path TEXT NOT NULL, source_line INTEGER, message_id TEXT, session_id TEXT, turn_id TEXT, role TEXT, content TEXT NOT NULL, timestamp TEXT);
PRAGMA user_version = 1;
