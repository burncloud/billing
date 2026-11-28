ALTER TABLE entries RENAME COLUMN request TO reference;

ALTER TABLE entries ADD COLUMN reference_kind TEXT NOT NULL DEFAULT 'request';

ALTER TABLE entries ALTER COLUMN reference_kind DROP DEFAULT;

ALTER TABLE entries DROP CONSTRAINT entries_account_kind_check;

ALTER TABLE entries DROP CONSTRAINT entries_check;

ALTER TABLE entries ADD CONSTRAINT entries_reference_kind_check CHECK (reference_kind IN ('request', 'credit'));

ALTER TABLE entries ADD CONSTRAINT entries_account_kind_check CHECK (account_kind IN ('available', 'held', 'revenue', 'funding'));

ALTER TABLE entries ADD CONSTRAINT entries_account_id_check CHECK (
    (account_kind IN ('revenue', 'funding') AND account_id IS NULL)
    OR (account_kind IN ('available', 'held') AND account_id IS NOT NULL)
);

ALTER TABLE entries ADD CONSTRAINT entries_credit_shape_check CHECK (
    (reference_kind = 'request' AND account_kind <> 'funding')
    OR (
        reference_kind = 'credit'
        AND entry_key IS NULL
        AND (
            (account_kind = 'funding' AND direction = 'debit')
            OR (account_kind = 'available' AND direction = 'credit')
        )
    )
);

CREATE UNIQUE INDEX entries_credit_once ON entries (reference, account_id)
WHERE reference_kind = 'credit' AND account_kind = 'available';

INSERT INTO entries (reference, reference_kind, account_kind, account_id, entry_key, direction, currency, amount, at)
SELECT c.credit_id, 'credit', v.account_kind, v.account_id, NULL, v.direction, c.currency, c.amount, c.at
FROM credits AS c
CROSS JOIN LATERAL (
    VALUES ('funding', NULL::TEXT, 'debit'), ('available', c.account, 'credit')
) AS v (account_kind, account_id, direction)
ORDER BY c.at, c.account COLLATE "C", c.credit_id COLLATE "C", v.account_kind DESC;
