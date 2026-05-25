use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub discord_client_id: String,
    pub discord_client_secret: String,
    pub discord_redirect_uri: String,
    pub discord_bot_token: String,
    pub port: u16,
    pub r2_bucket: String,
    pub r2_endpoint: String,
    pub r2_access_key_id: String,
    pub r2_secret_access_key: String,
    pub r2_public_url: String,
    #[allow(dead_code)]
    pub predetermined_channels: Option<String>,
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

        let discord_bot_token = env::var("DISCORD_BOT_TOKEN")
            .expect("DISCORD_BOT_TOKEN environment variable must be set");

        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(8080);

        let r2_bucket = env::var("R2_BUCKET")
            .expect("R2_BUCKET environment variable must be set");

        let r2_endpoint = env::var("R2_ENDPOINT")
            .expect("R2_ENDPOINT environment variable must be set");

        let r2_access_key_id = env::var("R2_ACCESS_KEY_ID")
            .expect("R2_ACCESS_KEY_ID environment variable must be set");

        let r2_secret_access_key = env::var("R2_SECRET_ACCESS_KEY")
            .expect("R2_SECRET_ACCESS_KEY environment variable must be set");

        let r2_public_url = env::var("R2_PUBLIC_URL")
            .expect("R2_PUBLIC_URL environment variable must be set");

        let predetermined_channels = env::var("PREDETERMINED_CHANNELS").ok();

        Self {
            database_url,
            discord_client_id,
            discord_client_secret,
            discord_redirect_uri,
            discord_bot_token,
            port,
            r2_bucket,
            r2_endpoint,
            r2_access_key_id,
            r2_secret_access_key,
            r2_public_url,
            predetermined_channels,
        }
    }
}
