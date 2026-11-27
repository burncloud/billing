CREATE TABLE balances (
    account TEXT NOT NULL,
    currency TEXT NOT NULL CHECK (currency IN ('usd', 'cny')),
    available BIGINT NOT NULL CHECK (available >= 0),
    held BIGINT NOT NULL CHECK (held >= 0),
    deposited BIGINT NOT NULL CHECK (deposited >= 0),
    PRIMARY KEY (account, currency)
);

CREATE TABLE holds (
    account TEXT NOT NULL,
    request TEXT NOT NULL,
    hold_key TEXT NOT NULL,
    currency TEXT NOT NULL CHECK (currency IN ('usd', 'cny')),
    amount BIGINT NOT NULL CHECK (amount >= 0),
    state TEXT NOT NULL CHECK (state IN ('open', 'settled', 'released')),
    PRIMARY KEY (account, request)
);

CREATE TABLE entries (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    request TEXT NOT NULL,
    account_kind TEXT NOT NULL CHECK (account_kind IN ('available', 'held', 'revenue')),
    account_id TEXT NULL,
    entry_key TEXT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('debit', 'credit')),
    currency TEXT NOT NULL CHECK (currency IN ('usd', 'cny')),
    amount BIGINT NOT NULL CHECK (amount >= 0),
    at BIGINT NOT NULL CHECK (at >= 0),
    CHECK (
        (account_kind = 'revenue' AND account_id IS NULL)
        OR (account_kind IN ('available', 'held') AND account_id IS NOT NULL)
    )
);
