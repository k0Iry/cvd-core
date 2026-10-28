use std::{future::Future, path::Path, sync::Arc, time::Duration};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    PgPool, SqlitePool,
};
use tokio::sync::Notify;

use crate::{
    models::BarRow,
    types::{Exchange, Market, Symbol, Tf},
    Bar,
};

const MAX_PENDING_BARS: i64 = 100_000;

#[derive(Debug, Serialize, Deserialize)]
pub struct PendingBar {
    exchange: String,
    market: String,
    symbol: String,
    tf: String,
    bar: BarRow,
}

impl PendingBar {
    pub fn new(exchange: Exchange, market: Market, symbol: Symbol, tf: Tf, bar: &Bar) -> Self {
        Self {
            exchange: exchange.as_str().into(),
            market: market.as_str().into(),
            symbol: symbol.as_str().into(),
            tf: tf.as_str().into(),
            bar: BarRow::from(bar),
        }
    }
}

pub struct Outbox {
    pub(crate) pool: SqlitePool,
    notify: Notify,
}

impl Outbox {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await?;
        }
        Self::connect(SqliteConnectOptions::new().filename(path).create_if_missing(true)).await
    }

    async fn connect(options: SqliteConnectOptions) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.journal_mode(SqliteJournalMode::Wal).synchronous(SqliteSynchronous::Full))
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS pending_bars (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT NOT NULL)",
        )
        .execute(&pool)
        .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS retired_pending_bars (id INTEGER PRIMARY KEY, payload TEXT NOT NULL)")
            .execute(&pool)
            .await?;
        let mut transaction = pool.begin().await?;
        sqlx::query("INSERT INTO retired_pending_bars (id, payload) SELECT id, payload FROM pending_bars WHERE CASE WHEN json_valid(payload) THEN json_extract(payload, '$.exchange') END = 'bybit' OR (CASE WHEN json_valid(payload) THEN json_extract(payload, '$.exchange') END = 'coinbase' AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.market') END = 'usdm')")
            .execute(&mut *transaction).await?;
        sqlx::query("DELETE FROM pending_bars WHERE CASE WHEN json_valid(payload) THEN json_extract(payload, '$.exchange') END = 'bybit' OR (CASE WHEN json_valid(payload) THEN json_extract(payload, '$.exchange') END = 'coinbase' AND CASE WHEN json_valid(payload) THEN json_extract(payload, '$.market') END = 'usdm')")
            .execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(Self { pool, notify: Notify::new() })
    }

    #[cfg(test)]
    pub async fn memory() -> Self {
        Self::connect(SqliteConnectOptions::new().in_memory(true)).await.unwrap()
    }

    #[cfg(test)]
    async fn enqueue(&self, bar: &PendingBar) -> Result<()> {
        self.enqueue_batch(std::slice::from_ref(bar)).await
    }

    pub async fn enqueue_batch(&self, bars: &[PendingBar]) -> Result<()> {
        self.enqueue_with_limit(bars, MAX_PENDING_BARS).await
    }

    async fn enqueue_with_limit(&self, bars: &[PendingBar], limit: i64) -> Result<()> {
        if bars.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            bars.iter().all(|bar| !(bar.exchange == "coinbase" && bar.market == "usdm")),
            "retired Coinbase INTX bars cannot enter the active outbox"
        );
        let payloads: Vec<_> = bars
            .iter()
            .map(|pending| {
                let bar = &pending.bar;
                anyhow::ensure!(
                    bar.trades >= 0 && crate::Bar::from(bar.clone()).is_renderable(),
                    "invalid bar cannot enter durable outbox"
                );
                Ok(serde_json::to_string(pending)?)
            })
            .collect::<Result<_>>()?;
        let mut transaction = self.pool.begin().await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_bars").fetch_one(&mut *transaction).await?;
        anyhow::ensure!(count + payloads.len() as i64 <= limit, "outbox full; ingestion is waiting for persistence");
        for payload in payloads {
            sqlx::query("INSERT INTO pending_bars (payload) VALUES (?)")
                .bind(payload)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        self.notify.notify_one();
        Ok(())
    }

    pub async fn pending_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM pending_bars").fetch_one(&self.pool).await?)
    }

    pub async fn retired_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM retired_pending_bars").fetch_one(&self.pool).await?)
    }

    async fn deliver_one<Writer, WriteFuture>(&self, writer: Writer) -> Result<bool>
    where
        Writer: FnOnce(PendingBar) -> WriteFuture,
        WriteFuture: Future<Output = Result<()>>,
    {
        let row: Option<(i64, String)> = sqlx::query_as("SELECT id, payload FROM pending_bars ORDER BY id LIMIT 1")
            .fetch_optional(&self.pool)
            .await?;
        let Some((id, payload)) = row else {
            return Ok(false);
        };
        writer(serde_json::from_str(&payload)?).await?;
        sqlx::query("DELETE FROM pending_bars WHERE id = ?").bind(id).execute(&self.pool).await?;
        Ok(true)
    }

    pub async fn drain(&self, db: &PgPool) -> Result<()> {
        while self.deliver_one(|bar| persist_bar(db, bar)).await? {}
        Ok(())
    }

    pub async fn run(self: Arc<Self>, db: PgPool) {
        let mut retry_seconds = 1;
        loop {
            match self.deliver_one(|bar| persist_bar(&db, bar)).await {
                Ok(true) => retry_seconds = 1,
                Ok(false) => self.notify.notified().await,
                Err(error) => {
                    eprintln!("outbox delivery failed; retained for retry: {error}");
                    tokio::time::sleep(Duration::from_secs(retry_seconds)).await;
                    retry_seconds = (retry_seconds * 2).min(30);
                }
            }
        }
    }
}

async fn persist_bar(db: &PgPool, pending: PendingBar) -> Result<()> {
    let bar = pending.bar;
    sqlx::query(
        r#"
        INSERT INTO bars (
            exchange, market, symbol, tf, time,
            price_open, price_high, price_low, price_close,
            delta_usdt, cvd_usdt, buy_volume_base, sell_volume_base,
            buy_volume_usdt, sell_volume_usdt, trades
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)
        ON CONFLICT(exchange, market, symbol, tf, time) DO UPDATE SET
            price_open=excluded.price_open, price_high=excluded.price_high,
            price_low=excluded.price_low, price_close=excluded.price_close,
            delta_usdt=excluded.delta_usdt, cvd_usdt=excluded.cvd_usdt,
            buy_volume_base=excluded.buy_volume_base, sell_volume_base=excluded.sell_volume_base,
            buy_volume_usdt=excluded.buy_volume_usdt, sell_volume_usdt=excluded.sell_volume_usdt,
            trades=excluded.trades
    "#,
    )
    .bind(pending.exchange)
    .bind(pending.market)
    .bind(pending.symbol)
    .bind(pending.tf)
    .bind(bar.time)
    .bind(bar.price_open)
    .bind(bar.price_high)
    .bind(bar.price_low)
    .bind(bar.price_close)
    .bind(bar.delta_usdt)
    .bind(bar.cvd_usdt)
    .bind(bar.buy_volume_base)
    .bind(bar.sell_volume_base)
    .bind(bar.buy_volume_usdt)
    .bind(bar.sell_volume_usdt)
    .bind(bar.trades)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retired_outbox_is_preserved_without_delivery_or_blocking_active_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("outbox.sqlite");
        let outbox = Outbox::open(&path).await.unwrap();
        let active =
            PendingBar::new(Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &crate::tests::bar(59, 12.0));
        let mut retired = serde_json::to_value(&active).unwrap();
        retired["exchange"] = "bybit".into();
        let payload = serde_json::to_string(&retired).unwrap();
        sqlx::query("INSERT INTO pending_bars(payload) VALUES (?)").bind(&payload).execute(&outbox.pool).await.unwrap();
        outbox.enqueue(&active).await.unwrap();
        outbox.pool.close().await;
        let reopened = Outbox::open(&path).await.unwrap();
        assert_eq!(reopened.pending_count().await.unwrap(), 1);
        let saved: String =
            sqlx::query_scalar("SELECT payload FROM retired_pending_bars").fetch_one(&reopened.pool).await.unwrap();
        assert_eq!(saved, payload);
        assert!(reopened
            .deliver_one(|pending| async move {
                assert_eq!(pending.exchange, "binance");
                assert_eq!(pending.bar.cvd_usdt, 12.0);
                Ok(())
            })
            .await
            .unwrap());
        reopened.pool.close().await;
        let again = Outbox::open(&path).await.unwrap();
        assert_eq!(again.pending_count().await.unwrap(), 0);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM retired_pending_bars").fetch_one(&again.pool).await.unwrap();
        assert_eq!(count, 1);
        again.enqueue_with_limit(&[active], 1).await.unwrap();
        assert!(again.deliver_one(|_| async { Ok(()) }).await.unwrap());
        let mut state = crate::tests::state().await;
        state.outbox = Arc::new(again);
        state.ready.store(true, std::sync::atomic::Ordering::Release);
        for shard in state.shards.values() {
            *shard.stream_error.write().await = None;
            shard.recovery.write().await.state = "live".into();
            shard.engine.lock().await.last_trade_ms = Some(chrono::Utc::now().timestamp_millis());
        }
        use axum::response::IntoResponse;
        let response = crate::health(axum::extract::State(Arc::new(state))).await.into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 100_000).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["healthy"], true);
        assert_eq!(value["retired_pending_bars"], 1);
    }

    #[tokio::test]
    async fn multi_timeframe_batch_is_atomic_and_invalid_numbers_never_poison_queue() {
        let outbox = Outbox::memory().await;
        let bars: Vec<_> = [Tf::M1, Tf::M5]
            .into_iter()
            .map(|tf| PendingBar::new(Exchange::Binance, Market::Spot, Symbol::Btc, tf, &crate::tests::bar(299, 42.0)))
            .collect();
        assert!(outbox.enqueue_with_limit(&bars, 1).await.is_err());
        assert_eq!(outbox.pending_count().await.unwrap(), 0);
        outbox.enqueue_with_limit(&bars, 2).await.unwrap();
        assert_eq!(outbox.pending_count().await.unwrap(), 2);
        let mut invalid = crate::tests::bar(359, 43.0);
        invalid.delta_usdt = f64::INFINITY;
        assert!(outbox
            .enqueue(&PendingBar::new(Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &invalid))
            .await
            .is_err());
        assert_eq!(outbox.pending_count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn failed_delivery_survives_reopen_and_is_acknowledged_only_on_success() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("outbox.sqlite");
        let outbox = Outbox::open(&path).await.unwrap();
        let bar = PendingBar::new(Exchange::Binance, Market::Spot, Symbol::Btc, Tf::M1, &crate::tests::bar(59, 42.0));
        outbox.enqueue(&bar).await.unwrap();
        assert!(outbox.deliver_one(|_| async { anyhow::bail!("database unavailable") }).await.is_err());
        outbox.pool.close().await;
        let reopened = Outbox::open(&path).await.unwrap();
        assert!(reopened
            .deliver_one(|pending| async move {
                assert_eq!(pending.bar.time, 59);
                assert_eq!(pending.bar.cvd_usdt, 42.0);
                Ok(())
            })
            .await
            .unwrap());
        assert!(!reopened.deliver_one(|_| async { panic!("already acknowledged") }).await.unwrap());
    }

    #[tokio::test]
    async fn queue_keeps_order_and_does_not_drop_newer_revisions() {
        let outbox = Outbox::memory().await;
        for cvd in [10.0, 20.0] {
            outbox
                .enqueue(&PendingBar::new(
                    Exchange::Binance,
                    Market::Spot,
                    Symbol::Btc,
                    Tf::M1,
                    &crate::tests::bar(59, cvd),
                ))
                .await
                .unwrap();
        }
        for expected in [10.0, 20.0] {
            assert!(outbox
                .deliver_one(|pending| async move {
                    assert_eq!(pending.bar.cvd_usdt, expected);
                    Ok(())
                })
                .await
                .unwrap());
        }
    }
}
