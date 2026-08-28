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

    #[error("No matching liquidity")]
    NoMatchingLiquidity,
}

impl From<archer_sdk::onchain::ArcherError> for ArcherAmmError {
    fn from(e: archer_sdk::onchain::ArcherError) -> Self {
        ArcherAmmError::MathError(format!("{e:?}"))
    }
}
