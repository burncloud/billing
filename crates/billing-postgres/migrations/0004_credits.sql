CREATE TABLE credits (
    account TEXT NOT NULL,
    credit_id TEXT NOT NULL CHECK (credit_id <> ''),
    currency TEXT NOT NULL CHECK (currency = 'usd'),
    amount BIGINT NOT NULL CHECK (amount > 0),
    at BIGINT NOT NULL CHECK (at >= 0),
    PRIMARY KEY (account, credit_id),
    FOREIGN KEY (account, currency) REFERENCES balances (account, currency)
);
