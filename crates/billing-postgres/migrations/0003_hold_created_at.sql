ALTER TABLE holds ADD COLUMN created_at BIGINT;

UPDATE holds
SET created_at = COALESCE(
    (
        SELECT MIN(entries.at)
        FROM entries
        WHERE entries.request = holds.request
            AND entries.account_kind = 'held'
            AND entries.account_id = holds.account
    ),
    CASE WHEN holds.amount = 0 THEN 0 END
);

ALTER TABLE holds ALTER COLUMN created_at SET NOT NULL;

ALTER TABLE holds ADD CONSTRAINT holds_created_at_not_negative CHECK (created_at >= 0);

CREATE INDEX holds_open_created_at ON holds (created_at) WHERE state = 'open';
