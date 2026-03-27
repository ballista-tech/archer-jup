use thiserror::Error;

#[derive(Error, Debug)]
pub enum ArcherAmmError {
    #[error("Deserialization failed: {0}")]
    DeserializationFailed(String),

    #[error("Missing state: {0}")]
    MissingState(String),

    #[error("Math error: {0}")]
    MathError(String),

    #[error("Market not active")]
    MarketNotActive,

    #[error("Async swap not supported")]
    AsyncNotSupported,

    #[error("No matching liquidity")]
    NoMatchingLiquidity,
}
