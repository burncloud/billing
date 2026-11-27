CREATE TABLE model_prices (
    model TEXT PRIMARY KEY CHECK (model <> ''),
    currency TEXT NOT NULL CHECK (currency = 'usd'),
    input_nanos_per_mtok BIGINT NOT NULL CHECK (input_nanos_per_mtok > 0),
    cached_input_nanos_per_mtok BIGINT NOT NULL CHECK (cached_input_nanos_per_mtok > 0),
    output_nanos_per_mtok BIGINT NOT NULL CHECK (output_nanos_per_mtok > 0),
    updated_at BIGINT NOT NULL CHECK (updated_at >= 0),
    CONSTRAINT model_prices_cached_not_above_input CHECK (cached_input_nanos_per_mtok <= input_nanos_per_mtok)
);
