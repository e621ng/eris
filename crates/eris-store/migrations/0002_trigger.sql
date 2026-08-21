-- Event-log trigger on `images`.
--
-- The advisory xact lock (key = 0x45524953, 'ERIS') is held until commit,
-- which serializes event insertion so that seq order equals commit order.
-- That invariant is what makes both cursor tailing (`WHERE seq > $cursor`)
-- and the bootstrap snapshot (`max(seq)` inside REPEATABLE READ) gap-free.
-- At ERIS write rates the serialization is free; bulk imports must suppress
-- the trigger via `SELECT set_config('eris.skip_events', 'on', false)` on
-- their session or the lock would serialize the whole load.
CREATE FUNCTION eris_images_event() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  -- current_setting with missing_ok yields NULL when unset; NULL = 'on' is
  -- NULL, so events fire by default on fresh connections.
  IF current_setting('eris.skip_events', true) = 'on' THEN
    RETURN NULL;
  END IF;

  PERFORM pg_advisory_xact_lock(1163086163);

  IF TG_OP = 'DELETE' THEN
    INSERT INTO image_events (op, post_id) VALUES (2, OLD.post_id);
  ELSE
    INSERT INTO image_events (op, post_id, avglf1, avglf2, avglf3, sig)
    VALUES (1, NEW.post_id, NEW.avglf1, NEW.avglf2, NEW.avglf3, NEW.sig);
  END IF;

  PERFORM pg_notify('eris_events', '');
  RETURN NULL;
END
$$;

CREATE TRIGGER images_event
  AFTER INSERT OR UPDATE OR DELETE ON images
  FOR EACH ROW EXECUTE FUNCTION eris_images_event();
