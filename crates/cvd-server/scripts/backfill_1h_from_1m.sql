-- One-time/manual 1h history backfill from existing 1m bars.
--
-- Usage example:
--   psql "$DATABASE_URL" -f scripts/backfill_1h_from_1m.sql
--
-- This intentionally lives outside the /history request path: opening the chart
-- should not silently scan 1m history and write DB rows. Run this once after
-- deploying 1h support, or rerun safely later; ON CONFLICT keeps it idempotent.
-- Coinbase INTX usdm is retired; do not backfill it or treat it as DRB usdc.
-- This is a manual script, not an automatic pre-adoption DRB history backfill.
--
-- Optional narrowing: add predicates such as
--   AND exchange = 'binance' AND market = 'spot' AND symbol = 'BTC'
-- inside the minute_bars CTE WHERE clause below before running.

WITH minute_bars AS (
    SELECT
        exchange,
        market,
        symbol,
        time,
        price_open,
        price_high,
        price_low,
        price_close,
        delta_usdt,
        cvd_usdt,
        buy_volume_base,
        sell_volume_base,
        buy_volume_usdt,
        sell_volume_usdt,
        trades,
        (time / 3600) * 3600 + 3599 AS hour_end
    FROM bars
    WHERE tf = '1m'
      AND symbol IN ('BTC', 'ETH')
      AND ((exchange = 'binance' AND market IN ('spot', 'usdm', 'coinm'))
        OR (exchange = 'coinbase' AND market IN ('spot', 'usdc')))
      -- Skip the still-forming current hour.
      AND time < (EXTRACT(EPOCH FROM date_trunc('hour', now()))::BIGINT)
), hourly AS (
    SELECT
        exchange,
        market,
        symbol,
        hour_end,
        COUNT(*) AS bars_in_hour,
        (array_agg(price_open ORDER BY time ASC))[1] AS price_open,
        MAX(price_high) AS price_high,
        MIN(price_low) AS price_low,
        (array_agg(price_close ORDER BY time DESC))[1] AS price_close,
        SUM(delta_usdt) AS delta_usdt,
        (array_agg(cvd_usdt ORDER BY time DESC))[1] AS cvd_usdt,
        SUM(buy_volume_base) AS buy_volume_base,
        SUM(sell_volume_base) AS sell_volume_base,
        SUM(buy_volume_usdt) AS buy_volume_usdt,
        SUM(sell_volume_usdt) AS sell_volume_usdt,
        SUM(trades)::BIGINT AS trades
    FROM minute_bars
    GROUP BY exchange, market, symbol, hour_end
)
INSERT INTO bars (
    exchange,
    market,
    symbol,
    tf,
    time,
    price_open,
    price_high,
    price_low,
    price_close,
    delta_usdt,
    cvd_usdt,
    buy_volume_base,
    sell_volume_base,
    buy_volume_usdt,
    sell_volume_usdt,
    trades
)
SELECT
    exchange,
    market,
    symbol,
    '1h' AS tf,
    hour_end AS time,
    price_open,
    price_high,
    price_low,
    price_close,
    delta_usdt,
    cvd_usdt,
    buy_volume_base,
    sell_volume_base,
    buy_volume_usdt,
    sell_volume_usdt,
    trades
FROM hourly
WHERE bars_in_hour = 60
ON CONFLICT (exchange, market, symbol, tf, time) DO NOTHING;

-- Quick verification after running:
--   SELECT exchange, market, symbol, COUNT(*) AS bars, MIN(time), MAX(time)
--   FROM bars
--   WHERE tf = '1h'
--   GROUP BY exchange, market, symbol
--   ORDER BY exchange, market, symbol;
