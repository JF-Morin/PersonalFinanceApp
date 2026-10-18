use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub storage_path: String,
    pub log_path: String,
    pub server_host: String,
    pub server_port: u16,
    pub cookie_secret: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Config {
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:./data/finance.db".to_string()),
            storage_path: env::var("STORAGE_PATH")
                .unwrap_or_else(|_| "./storage".to_string()),
            log_path: env::var("LOG_PATH")
                .unwrap_or_else(|_| "./logs".to_string()),
            server_host: env::var("SERVER_HOST")
                .unwrap_or_else(|_| "0.0.0.0".to_string()),
            server_port: env::var("SERVER_PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()?,
            cookie_secret: env::var("COOKIE_SECRET")
                .unwrap_or_else(|_| "change-this-secret-in-production-32bytes!!".to_string()),
        })
    }
}
