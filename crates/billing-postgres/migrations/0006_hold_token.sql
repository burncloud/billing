ALTER TABLE holds ADD COLUMN hold_token TEXT;

UPDATE holds
SET hold_token = gen_random_uuid()::text
WHERE hold_token IS NULL;

ALTER TABLE holds ALTER COLUMN hold_token SET NOT NULL;

ALTER TABLE holds ADD CONSTRAINT holds_token_not_empty CHECK (hold_token <> '');
