"""Build a point-in-time market file from yfinance, in the format of fixtures/market.json.

    python3 notebooks/yfinance_market.py --root /content/yf

writes a folder the backend can run on (`trade ... --root /content/yf`):

    config/strategy.toml     the repository's strategy, with this universe's rebalance dates
    config/task_types/       link to the repository's task types
    prompts/                 link to the repository's prompts
    fixtures/market.json     securities, annual financials, monthly prices, news and profiles
    run.json                 the latest date, the rebalance dates, and every assumption made
    raw/<TICKER>.json        what yfinance returned, so a rerun does not download again

Point-in-time rules, so nothing reaches a date before it was public:
- annual statements count as filed FILING_LAG_DAYS after the fiscal year ends;
- market value on a financial record is the price on its filing date times the shares then;
- price rows hold only what was known on their day, except forward_1m_return, which the
  backtest alone reads (and which is 0 where the month has not passed yet);
- news counts from its publication day; the company profile from the last trading day fetched.

yfinance has no insider buying, so it is 0. Items a company does not report are defaulted
and listed in run.json, so it is clear what the numbers rest on.
"""

import argparse
import datetime as dt
import json
import math
import numbers
import os
import re
import sys
import time
from pathlib import Path

FILING_LAG_DAYS = 75
TAX_RATE = 0.21
DEFAULT_TICKERS = [
    "AAPL", "MSFT", "GOOGL", "IBM", "INTC",  # technology
    "JNJ", "PFE", "MRK",                    # drug makers
    "XOM", "CVX",                           # oil and gas
    "KO", "PEP", "PG",                      # consumer staples
    "CAT", "DE",                            # machinery
    "WMT",                                  # retail
]
MONTH = 21   # trading days
YEAR = 252


# ---- Fetching (the only part that needs yfinance and pandas) ------------------------------

def _retry(call, what, tries=4):
    for attempt in range(tries):
        try:
            return call()
        except Exception as e:  # yfinance raises many kinds, rate limits among them
            if attempt == tries - 1:
                print(f"  {what}: gave up ({type(e).__name__}: {e})")
                return None
            time.sleep(2 ** (attempt + 1))


def _statement(frame):
    # {period end: {item: value}} with ISO dates, numbers only.
    if frame is None or getattr(frame, "empty", True):
        return {}
    out = {}
    for period, items in frame.to_dict().items():
        values = {k: float(v) for k, v in items.items()
                  if isinstance(v, numbers.Real) and math.isfinite(v)}
        if values:
            out[str(period)[:10]] = values
    return out


def _news(items):
    out = []
    for item in items or []:
        content = item.get("content") or item
        title = (content.get("title") or "").strip()
        summary = (content.get("summary") or "").strip()
        provider = content.get("provider") or {}
        publisher = provider.get("displayName") if isinstance(provider, dict) else None
        publisher = publisher or content.get("publisher") or "news"
        when = content.get("pubDate") or content.get("displayTime")
        if not when and content.get("providerPublishTime"):
            when = dt.datetime.fromtimestamp(content["providerPublishTime"], dt.timezone.utc).isoformat()
        if title and when:
            out.append({"title": title, "summary": summary, "publisher": publisher, "date": when[:10]})
    return out


def fetch(ticker, years):
    import yfinance as yf

    t = yf.Ticker(ticker)
    history = _retry(lambda: t.history(period=f"{years}y", auto_adjust=True), f"{ticker} prices")
    prices = []
    if history is not None and not history.empty:
        for day, row in history.iterrows():
            close, volume = float(row["Close"]), float(row["Volume"])
            if math.isfinite(close) and close > 0 and math.isfinite(volume):
                prices.append([str(day)[:10], close, volume])
    info = _retry(t.get_info, f"{ticker} profile") or {}
    keep = ["shortName", "longName", "industry", "sector", "beta", "sharesOutstanding",
            "longBusinessSummary"]
    return {
        "ticker": ticker,
        "fetched": dt.date.today().isoformat(),
        "info": {k: info.get(k) for k in keep},
        "prices": prices,
        "income": _statement(_retry(lambda: t.income_stmt, f"{ticker} income statement")),
        "balance": _statement(_retry(lambda: t.balance_sheet, f"{ticker} balance sheet")),
        "cashflow": _statement(_retry(lambda: t.cashflow, f"{ticker} cash flow")),
        "news": _news(_retry(lambda: t.news, f"{ticker} news")),
    }


# ---- Building (plain Python, testable without yfinance) ----------------------------------

def _first(items, names):
    for name in names:
        value = items.get(name)
        if value is not None and math.isfinite(value):
            return value
    return None


def _ratio(a, b):
    # a / b, or 1 (no change) when either side is missing or b is zero.
    if a is None or b is None or b == 0:
        return 1.0
    return a / b


def _shift_days(day, days):
    return (dt.date.fromisoformat(day) + dt.timedelta(days=days)).isoformat()


def _fundamentals(raw, period, previous, three_back, close_on, notes):
    """One annual financial record in the fixture's fields, or None if core items are missing."""
    inc, bal, cf = raw["income"][period], raw["balance"].get(period, {}), raw["cashflow"].get(period, {})
    pinc = raw["income"].get(previous, inc) if previous else inc
    pbal = raw["balance"].get(previous, bal) if previous else bal
    pcf = raw["cashflow"].get(previous, cf) if previous else cf

    def get(source, names, default=None, label=None):
        value = _first(source, names)
        if value is None and default is not None:
            notes.add(f"{label or names[0]} not reported; used {default}")
            return default
        return value

    revenue = get(inc, ["Total Revenue", "Operating Revenue"])
    net_income = get(inc, ["Net Income", "Net Income Common Stockholders"])
    total_assets = get(bal, ["Total Assets"])
    book_value = get(bal, ["Stockholders Equity", "Common Stock Equity"])
    cfo = get(cf, ["Operating Cash Flow", "Cash Flow From Continuing Operating Activities"])
    if None in (revenue, net_income, total_assets, book_value, cfo):
        notes.add(f"FY{period[:4]} skipped: revenue, net income, assets, equity or operating cash flow missing")
        return None

    cogs = _first(inc, ["Cost Of Revenue", "Reconciled Cost Of Revenue"])
    gross_profit = _first(inc, ["Gross Profit"])
    if gross_profit is None:
        gross_profit = revenue - cogs if cogs is not None else revenue
        notes.add("gross profit not reported; derived from revenue")
    ebit = get(inc, ["EBIT", "Operating Income"], default=net_income, label="EBIT")
    total_debt = get(bal, ["Total Debt"], default=0.0)
    cash = get(bal, ["Cash And Cash Equivalents", "Cash Cash Equivalents And Short Term Investments"],
               default=0.0, label="Cash")
    current_assets = get(bal, ["Current Assets"], default=total_assets * 0.4)
    current_liabilities = get(bal, ["Current Liabilities"], default=total_assets * 0.3)
    total_liabilities = get(bal, ["Total Liabilities Net Minority Interest"],
                            default=total_assets - book_value)
    shares = _first(bal, ["Ordinary Shares Number", "Share Issued"]) or raw["info"].get("sharesOutstanding")
    if not shares:
        notes.add(f"FY{period[:4]} skipped: shares outstanding missing")
        return None
    prev_shares = _first(pbal, ["Ordinary Shares Number", "Share Issued"]) or shares

    filing = _shift_days(period, FILING_LAG_DAYS)
    market_cap = close_on(filing) * shares
    beta = raw["info"].get("beta") or 1.0
    invested = book_value + total_debt - cash
    roic = ebit * (1 - TAX_RATE) / (invested if invested > 0 else total_assets)

    # Beneish M-score, eight ratios; each falls back to 1 (no change) when not reported.
    prev_revenue = _first(pinc, ["Total Revenue", "Operating Revenue"]) or revenue
    rec, prec = _first(bal, ["Accounts Receivable", "Receivables"]), _first(pbal, ["Accounts Receivable", "Receivables"])
    gm = gross_profit / revenue if revenue else None
    pgp = _first(pinc, ["Gross Profit"])
    pgm = pgp / prev_revenue if pgp is not None and prev_revenue else gm
    ppe, pppe = _first(bal, ["Net PPE"]), _first(pbal, ["Net PPE"])
    pca, pta = _first(pbal, ["Current Assets"]), _first(pbal, ["Total Assets"])
    dep = _first(cf, ["Depreciation And Amortization", "Depreciation Amortization Depletion"]) \
        or _first(inc, ["Reconciled Depreciation"])
    pdep = _first(pcf, ["Depreciation And Amortization", "Depreciation Amortization Depletion"]) \
        or _first(pinc, ["Reconciled Depreciation"])
    sga, psga = _first(inc, ["Selling General And Administration"]), _first(pinc, ["Selling General And Administration"])
    ptl = _first(pbal, ["Total Liabilities Net Minority Interest"])
    dsri = _ratio(rec / revenue if rec is not None else None, prec / prev_revenue if prec is not None else None)
    gmi = _ratio(pgm, gm)
    aqi = _ratio(1 - (current_assets + (ppe or 0)) / total_assets,
                 1 - ((pca or current_assets) + (pppe or ppe or 0)) / (pta or total_assets))
    sgi = _ratio(revenue, prev_revenue)
    depi = _ratio(pdep / (pdep + pppe) if pdep and pppe else None, dep / (dep + ppe) if dep and ppe else None)
    sgai = _ratio(sga / revenue if sga else None, psga / prev_revenue if psga else None)
    lvgi = _ratio(total_liabilities / total_assets, ptl / pta if ptl and pta else None)
    tata = (net_income - cfo) / total_assets
    beneish = (-4.84 + 0.92 * dsri + 0.528 * gmi + 0.404 * aqi + 0.892 * sgi + 0.115 * depi
               - 0.172 * sgai + 4.679 * tata - 0.327 * lvgi)

    revenue_3y_ago = _first(raw["income"].get(three_back, {}), ["Total Revenue", "Operating Revenue"]) \
        if three_back else None
    if revenue_3y_ago is None:
        oldest = min(raw["income"])
        revenue_3y_ago = _first(raw["income"][oldest], ["Total Revenue", "Operating Revenue"]) or revenue
        notes.add("growth for the earliest years uses the oldest revenue fetched (no data three years back)")

    return {
        "record_id": f"YF-{raw['ticker']}-{period}",
        "ticker": raw["ticker"],
        "market": "US",
        "industry": raw["info"].get("industry") or raw["info"].get("sector") or "Unknown",
        "period_end": period,
        "filing_date": filing,
        "book_value": book_value,
        "market_cap": market_cap,
        "enterprise_value": market_cap + total_debt - cash,
        "revenue": revenue,
        "revenue_3y_ago": revenue_3y_ago,
        "gross_profit": gross_profit,
        "ebit": ebit,
        "net_income": net_income,
        "interest_expense": abs(get(inc, ["Interest Expense", "Interest Expense Non Operating"], default=0.0)),
        "cfo": cfo,
        "capex": abs(get(cf, ["Capital Expenditure"], default=0.0)),
        "total_assets": total_assets,
        "prev_total_assets": _first(pbal, ["Total Assets"]) or total_assets,
        "current_assets": current_assets,
        "current_liabilities": current_liabilities,
        "total_debt": total_debt,
        "cash": cash,
        "retained_earnings": get(bal, ["Retained Earnings"], default=0.0),
        "total_liabilities": total_liabilities,
        "shares_out": shares,
        "prev_shares_out": prev_shares,
        "goodwill": get(bal, ["Goodwill"], default=0.0),
        "prev_goodwill": _first(pbal, ["Goodwill"]) or 0.0,
        "sbc_expense": get(cf, ["Stock Based Compensation"], default=0.0),
        "insider_net_buy_usd": 0.0,
        "beneish_m": max(-10.0, min(10.0, beneish)),
        "roic": max(-1.0, min(2.0, roic)),
        "wacc": max(0.06, min(0.12, 0.04 + 0.055 * beta)),
    }


def _price_rows(prices):
    """Month-end rows (and the last day), each with what was known that day."""
    days = [p[0] for p in prices]
    close = [p[1] for p in prices]
    dollars = [p[1] * p[2] for p in prices]
    rows = []
    for i, day in enumerate(days):
        if i + 1 < len(days) and days[i + 1][:7] == day[:7]:
            continue  # not the month's last trading day, nor the last day fetched
        window = range(max(1, i - 19), i + 1)
        traded = [dollars[j] for j in window if dollars[j] > 0]
        moves = [abs(close[j] / close[j - 1] - 1) / dollars[j] for j in window if dollars[j] > 0]
        rows.append({
            "trade_date": day,
            "close_price": close[i],
            "adv_usd": sum(traded) / len(traded) if traded else 0.0,
            "amihud_illiq": 1e6 * sum(moves) / len(moves) if moves else 0.0,
            "momentum_12_1": close[i - MONTH] / close[i - YEAR] - 1 if i >= YEAR else 0.0,
            "forward_1m_return": close[i + MONTH] / close[i] - 1 if i + MONTH < len(days) else 0.0,
            "has_forward": i + MONTH < len(days),
            "has_momentum": i >= YEAR,
            "is_halted": False,
            "is_st": False,
        })
    return rows


def build_market(raws, calibration):
    """(market.json content, run summary) from fetched data."""
    securities, financials, prices, filings = [], [], [], []
    notes_by_ticker, latest = {}, ""
    for raw in raws:
        ticker, notes = raw["ticker"], set()
        if len(raw["prices"]) < YEAR + MONTH or not raw["income"]:
            notes_by_ticker[ticker] = ["left out: under a year of prices or no statements"]
            continue
        days = [p[0] for p in raw["prices"]]

        def close_on(day, prices=raw["prices"]):
            earlier = [p[1] for p in prices if p[0] <= day]
            return earlier[-1] if earlier else prices[0][1]

        periods = sorted(raw["income"])
        records = []
        for k, period in enumerate(periods):
            previous = periods[k - 1] if k > 0 else None
            three_back = next((p for p in periods if p[:4] == str(int(period[:4]) - 3)), None)
            record = _fundamentals(raw, period, previous, three_back, close_on, notes)
            if record:
                records.append(record)
        if not records:
            notes_by_ticker[ticker] = sorted(notes) + ["left out: no usable statements"]
            continue
        info = raw["info"]
        securities.append({
            "ticker": ticker,
            "name": info.get("shortName") or info.get("longName") or ticker,
            "market": "US",
            "industry": records[-1]["industry"],
            "list_date": days[0],
        })
        financials.extend(records)
        for row in _price_rows(raw["prices"]):
            prices.append({"ticker": ticker, **{k: v for k, v in row.items() if not k.startswith("has_")}})
        latest = max(latest, days[-1])

        if info.get("longBusinessSummary"):
            # The profile describes the company now: public from the last day fetched, not before.
            filings.append({"record_id": f"YF-{ticker}-PROFILE", "ticker": ticker, "market": "US",
                            "period_end": days[-1], "filing_date": days[-1],
                            "section": "profile", "source": "yfinance profile",
                            "content": info["longBusinessSummary"]})
        for n, item in enumerate(raw["news"]):
            text = item["title"] + (". " + item["summary"] if item["summary"] else "")
            filings.append({"record_id": f"YF-{ticker}-NEWS-{n + 1}", "ticker": ticker, "market": "US",
                            "period_end": item["date"], "filing_date": item["date"], "section": "news",
                            "source": item["publisher"], "content": text})
        notes_by_ticker[ticker] = sorted(notes)

    # Rebalance on quarter ends with a known next month, a year of history, and filed statements.
    included = {s["ticker"] for s in securities}
    usable = {}
    for raw in raws:
        if raw["ticker"] not in included:
            continue
        for row in _price_rows(raw["prices"]):
            if row["trade_date"][5:7] in ("03", "06", "09", "12") and row["has_forward"] and row["has_momentum"]:
                usable.setdefault(row["trade_date"], set()).add(raw["ticker"])
    filed = lambda day: {f["ticker"] for f in financials if f["filing_date"] <= day}
    dates = sorted(d for d, ts in usable.items() if len(ts & filed(d)) >= 3)[-8:]

    market = {"securities": securities, "financials": financials, "prices": prices,
              "filings": filings, "agent_calibration": calibration}
    run = {
        "latest_as_of": latest,
        "rebalance_dates": dates,
        "tickers": sorted(included),
        "notes": notes_by_ticker,
        "assumptions": {
            "filing_lag_days": FILING_LAG_DAYS,
            "market_cap": "close on the filing date times the shares reported for the year",
            "insider_net_buy_usd": "not available from yfinance; 0",
            "wacc": "4% plus beta times 5.5%, kept between 6% and 12%",
            "roic": f"EBIT after a {TAX_RATE:.0%} tax rate, over equity plus debt minus cash",
            "forward_1m_return": "0 where the following month has not passed; only the backtest reads it",
        },
    }
    return market, run


def set_key(text, table, key, value):
    """In [table] (None for the top), replace `key = ...` with `key = value`."""
    out, section, done = [], None, False
    for line in text.split("\n"):
        header = re.match(r"\s*\[([^\]]+)\]", line)
        if header:
            section = header.group(1).strip()
        elif section == table and not done and re.match(rf"\s*{re.escape(key)}\s*=", line):
            line, done = f"{key} = {json.dumps(value)}", True
        out.append(line)
    if not done:
        raise KeyError(f"[{table}] has no {key}")
    return "\n".join(out)


def strategy_text(base, dates, positions):
    text = set_key(base, None, "rebalance_dates", dates)
    text = set_key(text, "research", "audit_tickers", [])
    text = set_key(text, "research", "max_positions", positions)
    text = set_key(text, "backtest", "top_n", positions)
    return set_key(text, "backtest", "oos_start_date", dates[len(dates) // 2])


def write_root(root, repo, market, run, positions):
    root, repo = Path(root), Path(repo)
    (root / "config").mkdir(parents=True, exist_ok=True)
    (root / "fixtures").mkdir(parents=True, exist_ok=True)
    base = (repo / "config/strategy.toml").read_text()
    (root / "config/strategy.toml").write_text(strategy_text(base, run["rebalance_dates"], positions))
    for link, target in [(root / "config/task_types", repo / "config/task_types"),
                         (root / "prompts", repo / "prompts")]:
        if not link.exists():
            link.symlink_to(target.resolve())
    # allow_nan=False: the backend's JSON parser would reject NaN, so fail here instead.
    with open(root / "fixtures/market.json", "w") as f:
        json.dump(market, f, indent=1, allow_nan=False)
    with open(root / "run.json", "w") as f:
        json.dump(run, f, indent=1, ensure_ascii=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", required=True, help="folder to write; pass it to trade --root")
    parser.add_argument("--repo", default=str(Path(__file__).resolve().parent.parent))
    parser.add_argument("--tickers", default=",".join(DEFAULT_TICKERS))
    parser.add_argument("--years", type=int, default=5, help="years of prices to fetch")
    parser.add_argument("--positions", type=int, default=5, help="positions per portfolio")
    parser.add_argument("--refresh", action="store_true", help="download again instead of the cache")
    args = parser.parse_args()

    raw_dir = Path(args.root) / "raw"
    raw_dir.mkdir(parents=True, exist_ok=True)
    raws = []
    for ticker in [t.strip().upper() for t in args.tickers.split(",") if t.strip()]:
        cached = raw_dir / f"{ticker}.json"
        if cached.exists() and not args.refresh:
            raws.append(json.loads(cached.read_text()))
            continue
        print(f"fetching {ticker}")
        raw = fetch(ticker, args.years)
        cached.write_text(json.dumps(raw))
        raws.append(raw)
        time.sleep(1)  # be gentle with Yahoo

    calibration = json.loads((Path(args.repo) / "fixtures/market.json").read_text())["agent_calibration"]
    market, run = build_market(raws, calibration)
    if len(run["tickers"]) < 3 or not run["rebalance_dates"]:
        sys.exit(f"not enough usable data: {json.dumps(run['notes'], ensure_ascii=False)}")
    write_root(args.root, args.repo, market, run, args.positions)
    print(f"\n{len(run['tickers'])} securities: {', '.join(run['tickers'])}")
    print(f"latest date: {run['latest_as_of']}")
    print(f"rebalance dates: {', '.join(run['rebalance_dates'])}")
    print(f"{len(market['financials'])} annual records, {len(market['prices'])} price rows, "
          f"{len(market['filings'])} news and profile texts")
    for ticker, notes in run["notes"].items():
        for note in notes:
            print(f"  {ticker}: {note}")
    print(f"\nwrote {os.path.join(args.root, 'fixtures/market.json')}")


if __name__ == "__main__":
    main()
