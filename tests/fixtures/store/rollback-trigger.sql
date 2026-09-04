CREATE TRIGGER fail_record_insert BEFORE INSERT ON records BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;
