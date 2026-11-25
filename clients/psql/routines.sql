-- Functions, triggers, privileges and roles as psql lists them.
CREATE TABLE notes (id serial PRIMARY KEY, body text, updated timestamptz);
CREATE FUNCTION add_one(n int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT n + 1';
CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.updated := now(); RETURN NEW; END $$;
CREATE TRIGGER notes_touch BEFORE UPDATE ON notes FOR EACH ROW EXECUTE FUNCTION touch();
CREATE PROCEDURE reset_notes() LANGUAGE sql AS 'DELETE FROM notes';
CREATE ROLE reader NOLOGIN;
GRANT SELECT ON notes TO reader;
\df
\sf add_one
\d notes
\dp notes
\du reader
\dg reader
SELECT add_one(41);
CALL reset_notes();
DROP OWNED BY reader;
DROP ROLE reader;
