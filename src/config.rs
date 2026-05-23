use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub discord_client_id: String,
    pub discord_client_secret: String,
    pub discord_redirect_uri: String,
    pub port: u16,
}

impl Config {
    pub fn from_env() -> Self {
        // Load .env file if present (useful in local development)
        dotenvy::dotenv().ok();

        let database_url = env::var("DATABASE_URL")
            .expect("DATABASE_URL environment variable must be set");

        let discord_client_id = env::var("DISCORD_CLIENT_ID")
            .expect("DISCORD_CLIENT_ID environment variable must be set");

        let discord_client_secret = env::var("DISCORD_CLIENT_SECRET")
            .expect("DISCORD_CLIENT_SECRET environment variable must be set");

        let discord_redirect_uri = env::var("DISCORD_REDIRECT_URI")
            .expect("DISCORD_REDIRECT_URI environment variable must be set");

        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(8080);

        Self {
            database_url,
            discord_client_id,
            discord_client_secret,
            discord_redirect_uri,
            port,
        }
    }
}
