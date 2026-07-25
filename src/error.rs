use thiserror::Error;

#[derive(Debug, Error)]
pub enum LauncherError {
    #[error("Сеть: {0}")]
    Network(String),

    #[error("Разбор данных: {0}")]
    Parse(String),

    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("Java не найдена. Установите Java 17+ (или 21) и перезапустите лаунчер.")]
    JavaNotFound,

    #[error("Версия не найдена: {0}")]
    VersionNotFound(String),

    #[error("Контрольная сумма не совпала для {path}: ожидали {expected}, получили {got}")]
    Checksum {
        path: String,
        expected: String,
        got: String,
    },

    #[error("{0}")]
    Other(String),
}
