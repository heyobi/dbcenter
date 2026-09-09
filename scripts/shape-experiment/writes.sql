-- Built by run.sh; see the comment at the top of it.
SET client_min_messages=warning;
-- current shape: INSTEAD OF triggers, as delta_manager writes them
CREATE OR REPLACE FUNCTION f_ins() RETURNS TRIGGER AS $$ BEGIN
  IF NEW.id IS NULL THEN NEW.id := gen_random_uuid(); END IF;
  INSERT INTO coll_adds VALUES (NEW.*); RETURN NEW; END; $$ LANGUAGE plpgsql;
CREATE OR REPLACE FUNCTION f_upd() RETURNS TRIGGER AS $$ BEGIN
  DELETE FROM coll_adds WHERE id = OLD.id;
  INSERT INTO coll_dels (id) VALUES (OLD.id) ON CONFLICT (id) DO NOTHING;
  INSERT INTO coll_adds VALUES (NEW.*); RETURN NEW; END; $$ LANGUAGE plpgsql;
CREATE OR REPLACE FUNCTION f_del() RETURNS TRIGGER AS $$ BEGIN
  INSERT INTO coll_dels (id) VALUES (OLD.id) ON CONFLICT (id) DO NOTHING;
  DELETE FROM coll_adds WHERE id = OLD.id; RETURN OLD; END; $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS t_ins ON coll; CREATE TRIGGER t_ins INSTEAD OF INSERT ON coll FOR EACH ROW EXECUTE FUNCTION f_ins();
DROP TRIGGER IF EXISTS t_upd ON coll; CREATE TRIGGER t_upd INSTEAD OF UPDATE ON coll FOR EACH ROW EXECUTE FUNCTION f_upd();
DROP TRIGGER IF EXISTS t_del ON coll; CREATE TRIGGER t_del INSTEAD OF DELETE ON coll FOR EACH ROW EXECUTE FUNCTION f_del();

-- partitioned shape: a BEFORE trigger to route writes into the adds partition,
-- an AFTER DELETE trigger to keep the tombstone the sync engine needs
CREATE OR REPLACE FUNCTION p_route() RETURNS TRIGGER AS $$ BEGIN
  NEW.part_key := 'adds';
  IF NEW.id IS NULL THEN NEW.id := gen_random_uuid(); END IF;
  RETURN NEW; END; $$ LANGUAGE plpgsql;
CREATE OR REPLACE FUNCTION p_tomb() RETURNS TRIGGER AS $$ BEGIN
  INSERT INTO part_dels (id) VALUES (OLD.id) ON CONFLICT (id) DO NOTHING;
  RETURN OLD; END; $$ LANGUAGE plpgsql;
DROP TABLE IF EXISTS part_dels; CREATE TABLE part_dels (id uuid PRIMARY KEY, deleted_at timestamptz DEFAULT now());
DROP TRIGGER IF EXISTS p_r ON part; CREATE TRIGGER p_r BEFORE INSERT OR UPDATE ON part FOR EACH ROW EXECUTE FUNCTION p_route();
DROP TRIGGER IF EXISTS p_t ON part; CREATE TRIGGER p_t AFTER DELETE ON part FOR EACH ROW EXECUTE FUNCTION p_tomb();
