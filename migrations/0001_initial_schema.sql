-- Portable ANSI SQL schema compatible with both SQLite and PostgreSQL.

CREATE TABLE IF NOT EXISTS pit_securities (
    ticker TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    market TEXT NOT NULL,
    industry TEXT NOT NULL,
    list_date TEXT NOT NULL,
    delist_date TEXT,
    delist_return DOUBLE PRECISION
);

CREATE TABLE IF NOT EXISTS pit_financials (
    record_id TEXT PRIMARY KEY,
    ticker TEXT NOT NULL REFERENCES pit_securities(ticker),
    market TEXT NOT NULL,
    industry TEXT NOT NULL,
    period_end TEXT NOT NULL,
    filing_date TEXT NOT NULL,
    book_value DOUBLE PRECISION NOT NULL,
    market_cap DOUBLE PRECISION NOT NULL,
    enterprise_value DOUBLE PRECISION NOT NULL,
    revenue DOUBLE PRECISION NOT NULL,
    revenue_3y_ago DOUBLE PRECISION NOT NULL,
    gross_profit DOUBLE PRECISION NOT NULL,
    ebit DOUBLE PRECISION NOT NULL,
    net_income DOUBLE PRECISION NOT NULL,
    interest_expense DOUBLE PRECISION NOT NULL,
    cfo DOUBLE PRECISION NOT NULL,
    capex DOUBLE PRECISION NOT NULL,
    total_assets DOUBLE PRECISION NOT NULL,
    prev_total_assets DOUBLE PRECISION NOT NULL,
    current_assets DOUBLE PRECISION NOT NULL,
    current_liabilities DOUBLE PRECISION NOT NULL,
    total_debt DOUBLE PRECISION NOT NULL,
    cash DOUBLE PRECISION NOT NULL,
    retained_earnings DOUBLE PRECISION NOT NULL,
    total_liabilities DOUBLE PRECISION NOT NULL,
    shares_out DOUBLE PRECISION NOT NULL,
    prev_shares_out DOUBLE PRECISION NOT NULL,
    goodwill DOUBLE PRECISION NOT NULL,
    prev_goodwill DOUBLE PRECISION NOT NULL,
    sbc_expense DOUBLE PRECISION NOT NULL,
    insider_net_buy_usd DOUBLE PRECISION NOT NULL,
    beneish_m DOUBLE PRECISION NOT NULL,
    roic DOUBLE PRECISION NOT NULL,
    wacc DOUBLE PRECISION NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pit_fin_ticker_filing
    ON pit_financials(ticker, filing_date, period_end);

CREATE TABLE IF NOT EXISTS pit_prices (
    ticker TEXT NOT NULL REFERENCES pit_securities(ticker),
    trade_date TEXT NOT NULL,
    close_price DOUBLE PRECISION NOT NULL,
    forward_1m_return DOUBLE PRECISION NOT NULL,
    adv_usd DOUBLE PRECISION NOT NULL,
    amihud_illiq DOUBLE PRECISION NOT NULL,
    momentum_12_1 DOUBLE PRECISION NOT NULL,
    is_halted BIGINT NOT NULL DEFAULT 0,
    is_st BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (ticker, trade_date)
);

CREATE TABLE IF NOT EXISTS pit_filings (
    record_id TEXT PRIMARY KEY,
    ticker TEXT NOT NULL REFERENCES pit_securities(ticker),
    market TEXT NOT NULL,
    period_end TEXT NOT NULL,
    filing_date TEXT NOT NULL,
    section TEXT NOT NULL,
    source TEXT NOT NULL,
    content TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pit_filings_ticker_date
    ON pit_filings(ticker, filing_date);
