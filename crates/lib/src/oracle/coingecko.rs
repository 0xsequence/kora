use super::{PriceOracle, PriceSource, TokenPrice};
use crate::{
    constant::{COINGECKO_API_URL, SOL_MINT},
    error::KoraError,
    sanitize_error,
    validator::math_validator,
};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use reqwest::{Client, StatusCode};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

const COINGECKO_AUTH_HEADER: &str = "x-cg-pro-api-key";

const COINGECKO_DEFAULT_CONFIDENCE: f64 = 0.95;

const MAX_REASONABLE_PRICE: f64 = 1_000_000.0;
const MIN_REASONABLE_PRICE: f64 = 0.000_000_001;

static GLOBAL_COINGECKO_API_KEY: Lazy<Arc<RwLock<Option<String>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

/// Get the global CoinGecko API key, falling back to environment variable
fn get_coingecko_api_key() -> Option<String> {
    let api_key_guard = GLOBAL_COINGECKO_API_KEY.read();
    api_key_guard.clone().or_else(|| std::env::var("COINGECKO_API_KEY").ok())
}

/// CoinGecko keys the response by the requested contract address, preserving the
/// original base58 casing.
type CoinGeckoResponse = HashMap<String, CoinGeckoPriceData>;

#[derive(Debug, Deserialize)]
struct CoinGeckoPriceData {
    usd: f64,
}

pub struct CoinGeckoPriceOracle {
    api_url: String,
    api_key: String,
}

impl CoinGeckoPriceOracle {
    pub fn new() -> Result<Self, KoraError> {
        let api_key = get_coingecko_api_key().ok_or_else(|| {
            log::error!("CoinGecko API key not found. Set COINGECKO_API_KEY environment variable.");
            KoraError::ConfigError(
                "CoinGecko API key not found. Set COINGECKO_API_KEY environment variable"
                    .to_string(),
            )
        })?;

        let api_url = Self::build_price_api_url(COINGECKO_API_URL);

        Ok(Self { api_url, api_key })
    }

    fn build_price_api_url(base_url: &str) -> String {
        let trimmed = base_url.trim_end_matches('/');
        format!("{trimmed}/simple/token_price/solana")
    }
}

#[async_trait::async_trait]
impl PriceOracle for CoinGeckoPriceOracle {
    async fn get_price(
        &self,
        client: &Client,
        mint_address: &str,
    ) -> Result<TokenPrice, KoraError> {
        let prices = self.get_prices(client, &[mint_address.to_string()]).await?;

        prices.get(mint_address).cloned().ok_or_else(|| {
            KoraError::RpcError(format!("No price data from CoinGecko for mint {mint_address}"))
        })
    }

    async fn get_prices(
        &self,
        client: &Client,
        mint_addresses: &[String],
    ) -> Result<HashMap<String, TokenPrice>, KoraError> {
        if mint_addresses.is_empty() {
            return Ok(HashMap::new());
        }

        self.fetch_prices_from_url(client, &self.api_url, mint_addresses, &self.api_key).await
    }
}

impl CoinGeckoPriceOracle {
    fn validate_price_data(price_data: &CoinGeckoPriceData, mint: &str) -> Result<(), KoraError> {
        let price = price_data.usd;

        math_validator::validate_division(price)?;

        // Sanity check: price should be within reasonable bounds
        if price > MAX_REASONABLE_PRICE {
            log::error!(
                "Price data for mint {} exceeds reasonable bounds: {} > {}",
                mint,
                price,
                MAX_REASONABLE_PRICE
            );
            return Err(KoraError::RpcError(format!(
                "Price data for mint {} exceeds reasonable bounds",
                mint
            )));
        }

        if price < MIN_REASONABLE_PRICE {
            log::error!(
                "Price data for mint {} below reasonable bounds: {} < {}",
                mint,
                price,
                MIN_REASONABLE_PRICE
            );
            return Err(KoraError::RpcError(format!(
                "Price data for mint {} below reasonable bounds",
                mint
            )));
        }

        Ok(())
    }

    async fn fetch_prices_from_url(
        &self,
        client: &Client,
        api_url: &str,
        mint_addresses: &[String],
        api_key: &str,
    ) -> Result<HashMap<String, TokenPrice>, KoraError> {
        if mint_addresses.is_empty() {
            return Ok(HashMap::new());
        }

        let mut all_mints = vec![SOL_MINT.to_string()];
        all_mints.extend_from_slice(mint_addresses);
        let ids = all_mints.join(",");

        let url = format!("{api_url}?contract_addresses={ids}&vs_currencies=usd");

        let request = client.get(&url).header(COINGECKO_AUTH_HEADER, api_key);

        let response = request.send().await.map_err(|e| {
            KoraError::RpcError(format!("CoinGecko API request failed: {}", sanitize_error!(e)))
        })?;

        if !response.status().is_success() {
            match response.status() {
                StatusCode::TOO_MANY_REQUESTS => {
                    return Err(KoraError::RateLimitExceeded);
                }
                _ => {
                    return Err(KoraError::RpcError(format!(
                        "CoinGecko API error: {}",
                        response.status()
                    )));
                }
            }
        }

        let coingecko_response: CoinGeckoResponse = response.json().await.map_err(|e| {
            KoraError::RpcError(format!(
                "Failed to parse CoinGecko response: {}",
                sanitize_error!(e)
            ))
        })?;

        // Get SOL price for conversion
        let sol_price = coingecko_response
            .get(SOL_MINT)
            .ok_or_else(|| KoraError::RpcError("No SOL price data from CoinGecko".to_string()))?;

        Self::validate_price_data(sol_price, SOL_MINT)?;

        // Convert all prices to SOL-denominated
        let mut result = HashMap::new();
        for mint_address in mint_addresses {
            if let Some(price_data) = coingecko_response.get(mint_address.as_str()) {
                Self::validate_price_data(price_data, mint_address)?;

                // Convert f64 USD prices to Decimal at API boundary
                let token_usd = Decimal::from_f64_retain(price_data.usd).ok_or_else(|| {
                    KoraError::RpcError(format!("Invalid token price for mint {mint_address}"))
                })?;
                let sol_usd = Decimal::from_f64_retain(sol_price.usd).ok_or_else(|| {
                    KoraError::RpcError("Invalid SOL price from CoinGecko".to_string())
                })?;

                let price_in_sol = token_usd / sol_usd;

                result.insert(
                    mint_address.clone(),
                    TokenPrice {
                        price: price_in_sol,
                        confidence: COINGECKO_DEFAULT_CONFIDENCE,
                        source: PriceSource::CoinGecko,
                        // CoinGecko is an off-chain aggregator and reports no Solana slot, so
                        // staleness cannot be verified. Config validation rejects CoinGecko
                        // combined with max_price_staleness_slots > 0.
                        block_id: None,
                    },
                );
            } else {
                log::error!("No price data for mint {mint_address} from CoinGecko");
                return Err(KoraError::RpcError(format!(
                    "No price data from CoinGecko for mint {mint_address}"
                )));
            }
        }

        if result.is_empty() {
            return Err(KoraError::RpcError("No price data from CoinGecko".to_string()));
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use serial_test::serial;

    const COINGECKO_MOCK_PATH: &str = "/simple/token_price/solana";

    #[test]
    #[serial]
    fn test_new_fails_without_api_key() {
        // Clear both the global state AND the environment variable
        std::env::remove_var("COINGECKO_API_KEY");
        {
            let mut api_key_guard = GLOBAL_COINGECKO_API_KEY.write();
            *api_key_guard = None;
        }

        let result = CoinGeckoPriceOracle::new();
        assert!(result.is_err());
        assert!(matches!(result.err(), Some(KoraError::ConfigError(_))));
    }

    #[tokio::test]
    #[serial]
    async fn test_coingecko_price_fetch_with_api_key() {
        {
            let mut api_key_guard = GLOBAL_COINGECKO_API_KEY.write();
            *api_key_guard = Some("test-api-key".to_string());
        }

        let mock_response = r#"{
            "So11111111111111111111111111111111111111112": {
                "usd": 100.0,
                "last_updated_at": 1785509500
            },
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": {
                "usd": 25.0,
                "last_updated_at": 1785509500
            }
        }"#;

        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", COINGECKO_MOCK_PATH)
            .match_header(COINGECKO_AUTH_HEADER, "test-api-key")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(mock_response)
            .create();

        let client = Client::new();
        let mut oracle = CoinGeckoPriceOracle::new().unwrap();
        oracle.api_url = format!("{}{}", server.url(), COINGECKO_MOCK_PATH);

        let result =
            oracle.get_price(&client, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").await;
        assert!(result.is_ok());
        let price = result.unwrap();
        // 25 USD / 100 USD per SOL = 0.25 SOL
        assert_eq!(price.price, Decimal::new(25, 2));
        assert_eq!(price.source, PriceSource::CoinGecko);
    }

    #[tokio::test]
    #[serial]
    async fn test_coingecko_price_has_no_block_id() {
        {
            let mut api_key_guard = GLOBAL_COINGECKO_API_KEY.write();
            *api_key_guard = Some("test-api-key".to_string());
        }

        let mock_response = r#"{
            "So11111111111111111111111111111111111111112": {
                "usd": 100.0,
                "last_updated_at": 1785509500
            }
        }"#;

        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", COINGECKO_MOCK_PATH)
            .match_header(COINGECKO_AUTH_HEADER, "test-api-key")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(mock_response)
            .create();

        let client = Client::new();
        let mut oracle = CoinGeckoPriceOracle::new().unwrap();
        oracle.api_url = format!("{}{}", server.url(), COINGECKO_MOCK_PATH);

        let price = oracle.get_price(&client, SOL_MINT).await.unwrap();
        assert!(
            price.block_id.is_none(),
            "CoinGecko reports no Solana slot, so block_id must stay None"
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_coingecko_missing_price_data_returns_error() {
        {
            let mut api_key_guard = GLOBAL_COINGECKO_API_KEY.write();
            *api_key_guard = Some("test-api-key".to_string());
        }

        let no_price_response = r#"{
            "So11111111111111111111111111111111111111112": {
                "usd": 100.0,
                "last_updated_at": 1785509500
            }
        }"#;

        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", COINGECKO_MOCK_PATH)
            .match_header(COINGECKO_AUTH_HEADER, "test-api-key")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(no_price_response)
            .create();

        let client = Client::new();
        let mut oracle = CoinGeckoPriceOracle::new().unwrap();
        oracle.api_url = format!("{}{}", server.url(), COINGECKO_MOCK_PATH);

        let result =
            oracle.get_price(&client, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").await;
        assert!(result.is_err());
        assert_eq!(
            result.err(),
            Some(KoraError::RpcError(
                "No price data from CoinGecko for mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
                    .to_string()
            ))
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_coingecko_rate_limit_429() {
        {
            let mut api_key_guard = GLOBAL_COINGECKO_API_KEY.write();
            *api_key_guard = Some("test-api-key".to_string());
        }

        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", COINGECKO_MOCK_PATH)
            .match_header(COINGECKO_AUTH_HEADER, "test-api-key")
            .match_query(Matcher::Any)
            .with_status(429)
            .with_body("Too Many Requests")
            .create();

        let client = Client::new();
        let mut oracle = CoinGeckoPriceOracle::new().unwrap();
        oracle.api_url = format!("{}{}", server.url(), COINGECKO_MOCK_PATH);

        let result = oracle.get_prices(&client, &[SOL_MINT.to_string()]).await;
        assert!(result.is_err());
        assert_eq!(result.err(), Some(KoraError::RateLimitExceeded));
    }

    #[tokio::test]
    #[serial]
    async fn test_coingecko_price_exceeds_max_bounds() {
        {
            let mut api_key_guard = GLOBAL_COINGECKO_API_KEY.write();
            *api_key_guard = Some("test-api-key".to_string());
        }

        // SOL price is normal, but the queried token price exceeds MAX_REASONABLE_PRICE
        let mock_response = r#"{
            "So11111111111111111111111111111111111111112": {
                "usd": 100.0
            },
            "TokenMintExceedsMaxBounds1111111111111111": {
                "usd": 9999999.0
            }
        }"#;

        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", COINGECKO_MOCK_PATH)
            .match_header(COINGECKO_AUTH_HEADER, "test-api-key")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(mock_response)
            .create();

        let client = Client::new();
        let mut oracle = CoinGeckoPriceOracle::new().unwrap();
        oracle.api_url = format!("{}{}", server.url(), COINGECKO_MOCK_PATH);

        let result = oracle
            .get_prices(&client, &["TokenMintExceedsMaxBounds1111111111111111".to_string()])
            .await;
        assert!(result.is_err());
        assert!(
            matches!(result.err(), Some(KoraError::RpcError(msg)) if msg.contains("exceeds reasonable bounds"))
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_coingecko_price_below_min_bounds() {
        {
            let mut api_key_guard = GLOBAL_COINGECKO_API_KEY.write();
            *api_key_guard = Some("test-api-key".to_string());
        }

        // SOL price is normal, but the queried token price is below MIN_REASONABLE_PRICE
        let mock_response = r#"{
            "So11111111111111111111111111111111111111112": {
                "usd": 100.0
            },
            "TokenMintBelowMinBounds11111111111111111111": {
                "usd": 0.0000000001
            }
        }"#;

        let mut server = Server::new_async().await;
        let _m = server
            .mock("GET", COINGECKO_MOCK_PATH)
            .match_header(COINGECKO_AUTH_HEADER, "test-api-key")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(mock_response)
            .create();

        let client = Client::new();
        let mut oracle = CoinGeckoPriceOracle::new().unwrap();
        oracle.api_url = format!("{}{}", server.url(), COINGECKO_MOCK_PATH);

        let result = oracle
            .get_prices(&client, &["TokenMintBelowMinBounds11111111111111111111".to_string()])
            .await;
        assert!(result.is_err());
        assert!(
            matches!(result.err(), Some(KoraError::RpcError(msg)) if msg.contains("below reasonable bounds"))
        );
    }
}
