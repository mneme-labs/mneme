use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("connection error: {0}")]
    Connection(#[from] anyhow::Error),
    #[error("key not found")]
    KeyNotFound,
    #[error("wrong type")]
    WrongType,
    #[error("pool exhausted")]
    PoolExhausted,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
