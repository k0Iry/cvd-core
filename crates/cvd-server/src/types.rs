// ==============================
// Types: Exchange / Market / Symbol / Tf
// ==============================

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Exchange {
    Binance,
    Coinbase,
}
impl Exchange {
    pub const ALL: [Self; 2] = [Self::Binance, Self::Coinbase];

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "binance" => Some(Self::Binance),
            "coinbase" => Some(Self::Coinbase),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Coinbase => "coinbase",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Market {
    Spot,
    Usdm,
    Usdc,
    Coinm,
}
impl Market {
    pub fn from_str(s: &str) -> Self {
        match s {
            "usdm" => Self::Usdm,
            "usdc" => Self::Usdc,
            "coinm" => Self::Coinm,
            _ => Self::Spot,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Usdm => "usdm",
            Self::Usdc => "usdc",
            Self::Coinm => "coinm",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Symbol {
    Btc,
    Eth,
    Sol,
    Bnb,
    Aave,
}
impl Symbol {
    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "ETH" => Self::Eth,
            "SOL" => Self::Sol,
            "BNB" => Self::Bnb,
            "AAVE" => Self::Aave,
            _ => Self::Btc,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Btc => "BTC",
            Self::Eth => "ETH",
            Self::Sol => "SOL",
            Self::Bnb => "BNB",
            Self::Aave => "AAVE",
        }
    }

    // Spot / USDM use <asset>USDT
    pub fn spot_usdm_stream(self) -> &'static str {
        match self {
            Self::Btc => "btcusdt",
            Self::Eth => "ethusdt",
            Self::Sol => "solusdt",
            Self::Bnb => "bnbusdt",
            Self::Aave => "aaveusdt",
        }
    }

    // COINM uses <asset>USD_PERP
    pub fn coinm_stream(self) -> &'static str {
        match self {
            Self::Btc => "btcusd_perp",
            Self::Eth => "ethusd_perp",
            Self::Sol => "solusd_perp",
            Self::Bnb => "bnbusd_perp",
            Self::Aave => "aaveusd_perp",
        }
    }

    pub fn coinbase_spot_product(self) -> Option<&'static str> {
        match self {
            Self::Btc => Some("BTC-USD"),
            Self::Eth => Some("ETH-USD"),
            _ => None,
        }
    }

    pub fn from_coinbase_spot_product(product_id: &str) -> Option<Self> {
        match product_id {
            "BTC-USD" => Some(Self::Btc),
            "ETH-USD" => Some(Self::Eth),
            _ => None,
        }
    }

    pub fn from_binance_stream_name(stream: &str, market: Market) -> Option<Self> {
        let base = stream.split('@').next()?;
        match market {
            Market::Coinm => match base {
                "btcusd_perp" => Some(Self::Btc),
                "ethusd_perp" => Some(Self::Eth),
                "solusd_perp" => Some(Self::Sol),
                "bnbusd_perp" => Some(Self::Bnb),
                "aaveusd_perp" => Some(Self::Aave),
                _ => None,
            },
            _ => match base {
                "btcusdt" => Some(Self::Btc),
                "ethusdt" => Some(Self::Eth),
                "solusdt" => Some(Self::Sol),
                "bnbusdt" => Some(Self::Bnb),
                "aaveusdt" => Some(Self::Aave),
                _ => None,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Tf {
    M1,
    M5,
    M15,
    M30,
    H1,
}
impl Tf {
    pub fn ms(self) -> i64 {
        match self {
            Tf::M1 => 60_000,
            Tf::M5 => 300_000,
            Tf::M15 => 900_000,
            Tf::M30 => 1_800_000,
            Tf::H1 => 3_600_000,
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "5m" => Tf::M5,
            "15m" => Tf::M15,
            "30m" => Tf::M30,
            "1h" => Tf::H1,
            _ => Tf::M1,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Tf::M1 => "1m",
            Tf::M5 => "5m",
            Tf::M15 => "15m",
            Tf::M30 => "30m",
            Tf::H1 => "1h",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tf;

    #[test]
    fn thirty_minutes_round_trips_through_the_timeframe_api() {
        let tf = Tf::from_str("30m");
        assert_eq!(tf, Tf::M30);
        assert_eq!(tf.ms(), 1_800_000);
        assert_eq!(tf.as_str(), "30m");
    }
}
