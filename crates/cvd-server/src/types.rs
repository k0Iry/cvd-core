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
    SpotUsdc,
    Usdm,
    UsdmUsdc,
    Usdc,
    Coinm,
}
impl Market {
    pub fn from_str(s: &str) -> Self {
        match s {
            "spot_usdc" => Self::SpotUsdc,
            "usdm" => Self::Usdm,
            "usdm_usdc" => Self::UsdmUsdc,
            "usdc" => Self::Usdc,
            "coinm" => Self::Coinm,
            _ => Self::Spot,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::SpotUsdc => "spot_usdc",
            Self::Usdm => "usdm",
            Self::UsdmUsdc => "usdm_usdc",
            Self::Usdc => "usdc",
            Self::Coinm => "coinm",
        }
    }
    pub fn is_spot(self) -> bool {
        matches!(self, Self::Spot | Self::SpotUsdc)
    }
}

/// Stable persisted source identity and native execution semantics. Binance
/// spot/linear q is base quantity; inverse q is face-value contract count.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Instrument {
    #[serde(serialize_with = "serialize_exchange")]
    pub exchange: Exchange,
    #[serde(serialize_with = "serialize_market")]
    pub market: Market,
    #[serde(serialize_with = "serialize_symbol")]
    pub symbol: Symbol,
    pub native_symbol: &'static str,
    pub category: &'static str,
    pub contract_kind: Option<&'static str>,
    pub quote_asset: &'static str,
    pub settlement_asset: Option<&'static str>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuantitySemantics {
    BaseAsset,
    InverseContracts,
}
impl Market {
    pub fn quantity_semantics(self) -> QuantitySemantics {
        if self == Self::Coinm {
            QuantitySemantics::InverseContracts
        } else {
            QuantitySemantics::BaseAsset
        }
    }
}
fn serialize_exchange<S: serde::Serializer>(x: &Exchange, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(x.as_str())
}
fn serialize_market<S: serde::Serializer>(x: &Market, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(x.as_str())
}
fn serialize_symbol<S: serde::Serializer>(x: &Symbol, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(x.as_str())
}

pub const INSTRUMENTS: &[Instrument] = &[
    Instrument {
        exchange: Exchange::Binance,
        market: Market::Spot,
        symbol: Symbol::Btc,
        native_symbol: "BTCUSDT",
        category: "spot",
        contract_kind: None,
        quote_asset: "USDT",
        settlement_asset: None,
    },
    Instrument {
        exchange: Exchange::Binance,
        market: Market::Spot,
        symbol: Symbol::Eth,
        native_symbol: "ETHUSDT",
        category: "spot",
        contract_kind: None,
        quote_asset: "USDT",
        settlement_asset: None,
    },
    Instrument {
        exchange: Exchange::Binance,
        market: Market::SpotUsdc,
        symbol: Symbol::Btc,
        native_symbol: "BTCUSDC",
        category: "spot",
        contract_kind: None,
        quote_asset: "USDC",
        settlement_asset: None,
    },
    Instrument {
        exchange: Exchange::Binance,
        market: Market::SpotUsdc,
        symbol: Symbol::Eth,
        native_symbol: "ETHUSDC",
        category: "spot",
        contract_kind: None,
        quote_asset: "USDC",
        settlement_asset: None,
    },
    Instrument {
        exchange: Exchange::Binance,
        market: Market::Usdm,
        symbol: Symbol::Btc,
        native_symbol: "BTCUSDT",
        category: "contract",
        contract_kind: Some("linear"),
        quote_asset: "USDT",
        settlement_asset: Some("USDT"),
    },
    Instrument {
        exchange: Exchange::Binance,
        market: Market::Usdm,
        symbol: Symbol::Eth,
        native_symbol: "ETHUSDT",
        category: "contract",
        contract_kind: Some("linear"),
        quote_asset: "USDT",
        settlement_asset: Some("USDT"),
    },
    Instrument {
        exchange: Exchange::Binance,
        market: Market::UsdmUsdc,
        symbol: Symbol::Btc,
        native_symbol: "BTCUSDC",
        category: "contract",
        contract_kind: Some("linear"),
        quote_asset: "USDC",
        settlement_asset: Some("USDC"),
    },
    Instrument {
        exchange: Exchange::Binance,
        market: Market::UsdmUsdc,
        symbol: Symbol::Eth,
        native_symbol: "ETHUSDC",
        category: "contract",
        contract_kind: Some("linear"),
        quote_asset: "USDC",
        settlement_asset: Some("USDC"),
    },
    Instrument {
        exchange: Exchange::Binance,
        market: Market::Coinm,
        symbol: Symbol::Btc,
        native_symbol: "BTCUSD_PERP",
        category: "contract",
        contract_kind: Some("inverse"),
        quote_asset: "USD",
        settlement_asset: Some("BTC"),
    },
    Instrument {
        exchange: Exchange::Binance,
        market: Market::Coinm,
        symbol: Symbol::Eth,
        native_symbol: "ETHUSD_PERP",
        category: "contract",
        contract_kind: Some("inverse"),
        quote_asset: "USD",
        settlement_asset: Some("ETH"),
    },
    Instrument {
        exchange: Exchange::Coinbase,
        market: Market::Spot,
        symbol: Symbol::Btc,
        native_symbol: "BTC-USD",
        category: "spot",
        contract_kind: None,
        quote_asset: "USD",
        settlement_asset: None,
    },
    Instrument {
        exchange: Exchange::Coinbase,
        market: Market::Spot,
        symbol: Symbol::Eth,
        native_symbol: "ETH-USD",
        category: "spot",
        contract_kind: None,
        quote_asset: "USD",
        settlement_asset: None,
    },
    Instrument {
        exchange: Exchange::Coinbase,
        market: Market::Usdc,
        symbol: Symbol::Btc,
        native_symbol: "BTC_USDC-PERPETUAL",
        category: "contract",
        contract_kind: Some("linear"),
        quote_asset: "USDC",
        settlement_asset: Some("USDC"),
    },
    Instrument {
        exchange: Exchange::Coinbase,
        market: Market::Usdc,
        symbol: Symbol::Eth,
        native_symbol: "ETH_USDC-PERPETUAL",
        category: "contract",
        contract_kind: Some("linear"),
        quote_asset: "USDC",
        settlement_asset: Some("USDC"),
    },
];

pub fn instrument(exchange: Exchange, market: Market, symbol: Symbol) -> Option<&'static Instrument> {
    INSTRUMENTS.iter().find(|i| (i.exchange, i.market, i.symbol) == (exchange, market, symbol))
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

    pub fn coinbase_spot_product(self) -> Option<&'static str> {
        instrument(Exchange::Coinbase, Market::Spot, self).map(|i| i.native_symbol)
    }

    pub fn from_coinbase_spot_product(product_id: &str) -> Option<Self> {
        INSTRUMENTS
            .iter()
            .find(|i| i.exchange == Exchange::Coinbase && i.market == Market::Spot && i.native_symbol == product_id)
            .map(|i| i.symbol)
    }

    pub fn from_binance_stream_name(stream: &str, market: Market) -> Option<Self> {
        INSTRUMENTS
            .iter()
            .find(|source| {
                source.exchange == Exchange::Binance
                    && source.market == market
                    && stream == format!("{}@aggTrade", source.native_symbol.to_ascii_lowercase())
            })
            .map(|source| source.symbol)
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
