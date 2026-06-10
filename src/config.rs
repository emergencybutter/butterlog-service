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
    /// Public base URL of this service (no trailing slash), used when building
    /// share/detail links. Overridable via PUBLIC_BASE_URL.
    pub public_base_url: String,
    #[allow(dead_code)]
    pub predetermined_channels: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        // Load .env file if present (useful in local development)
        dotenvy::dotenv().ok();

        // Collect every missing required variable so the operator sees them all at once,
        // rather than fixing them one failed startup at a time.
        let mut missing: Vec<&'static str> = Vec::new();
        let mut req = |key: &'static str| -> String {
            match env::var(key) {
                Ok(v) => v,
                Err(_) => {
                    missing.push(key);
                    String::new()
                }
            }
        };

        let database_url = req("DATABASE_URL");
        let discord_client_id = req("DISCORD_CLIENT_ID");
        let discord_client_secret = req("DISCORD_CLIENT_SECRET");
        let discord_redirect_uri = req("DISCORD_REDIRECT_URI");
        let discord_bot_token = req("DISCORD_BOT_TOKEN");
        let r2_bucket = req("R2_BUCKET");
        let r2_endpoint = req("R2_ENDPOINT");
        let r2_access_key_id = req("R2_ACCESS_KEY_ID");
        let r2_secret_access_key = req("R2_SECRET_ACCESS_KEY");
        let r2_public_url = req("R2_PUBLIC_URL");
        drop(req); // release the mutable borrow of `missing`

        if !missing.is_empty() {
            panic!(
                "Missing required environment variables: {}",
                missing.join(", ")
            );
        }

        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(8080);

        let predetermined_channels = env::var("PREDETERMINED_CHANNELS").ok();

        let public_base_url = env::var("PUBLIC_BASE_URL")
            .unwrap_or_else(|_| "https://butterlog.flyvoyager.net".to_string())
            .trim_end_matches('/')
            .to_string();

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
            public_base_url,
            predetermined_channels,
        }
    }
}
