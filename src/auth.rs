use serde::Deserialize;
use sqlx::PgPool;
use crate::error::AppError;

#[derive(Deserialize, Debug)]
pub struct TokenResponse {
    pub access_token: String,
}

#[derive(Deserialize, Debug)]
pub struct DiscordUser {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
}

/// Generates the Discord OAuth2 authorization URL
pub fn get_login_url(client_id: &str, redirect_uri: &str, state: Option<&str>) -> String {
    let mut params = vec![
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", "identify"),
    ];
    if let Some(s) = state {
        params.push(("state", s));
    }
    let url = reqwest::Url::parse_with_params("https://discord.com/oauth2/authorize", &params)
        .expect("Failed to build Discord Auth URL");
    url.to_string()
}

/// Exchanges the code parameter for an access token
pub async fn exchange_code(
    http_client: &reqwest::Client,
    code: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
) -> Result<String, AppError> {
    let params = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
    ];

    let response = http_client
        .post("https://discord.com/api/v10/oauth2/token")
        .form(&params)
        .send()
        .await?;

    if !response.status().is_success() {
        let err_body = response.text().await.unwrap_or_default();
        return Err(AppError::Auth(format!(
            "Failed to exchange code for token. Response: {}",
            err_body
        )));
    }

    let token_res: TokenResponse = response.json().await?;
    Ok(token_res.access_token)
}

/// Fetches the user profile from Discord API
pub async fn fetch_discord_user(
    http_client: &reqwest::Client,
    access_token: &str,
) -> Result<DiscordUser, AppError> {
    let response = http_client
        .get("https://discord.com/api/v10/users/@me")
        .bearer_auth(access_token)
        .send()
        .await?;

    if !response.status().is_success() {
        let err_body = response.text().await.unwrap_or_default();
        return Err(AppError::Auth(format!(
            "Failed to fetch Discord user details. Response: {}",
            err_body
        )));
    }

    let user: DiscordUser = response.json().await?;
    Ok(user)
}

/// Inserts or updates the user info in PostgreSQL and returns their API token
pub async fn save_or_update_user(
    db_pool: &PgPool,
    user: &DiscordUser,
) -> Result<String, AppError> {
    let api_token: String = sqlx::query_scalar(
        "INSERT INTO users (discord_id, username, global_name, avatar, api_token, last_login) \
         VALUES ($1, $2, $3, $4, md5(random()::text || clock_timestamp()::text), CURRENT_TIMESTAMP) \
         ON CONFLICT (discord_id) \
         DO UPDATE SET \
             username = EXCLUDED.username, \
             global_name = EXCLUDED.global_name, \
             avatar = EXCLUDED.avatar, \
             last_login = EXCLUDED.last_login \
         RETURNING api_token"
    )
    .bind(&user.id)
    .bind(&user.username)
    .bind(&user.global_name)
    .bind(&user.avatar)
    .fetch_one(db_pool)
    .await?;

    Ok(api_token)
}
