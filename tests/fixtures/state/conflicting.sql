CREATE TABLE threads (id TEXT, cwd TEXT, model TEXT);
INSERT INTO threads VALUES ('fixture-duplicate-session', '/first', 'model-one');
INSERT INTO threads VALUES ('fixture-duplicate-session', '/second', 'model-two');
