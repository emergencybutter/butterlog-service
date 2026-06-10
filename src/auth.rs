use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;
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

    tracing::info!("[Outgoing Request] POST https://discord.com/api/v10/oauth2/token");
    let response = http_client
        .post("https://discord.com/api/v10/oauth2/token")
        .form(&params)
        .send()
        .await?;
    tracing::info!("[Outgoing Response] {} for POST https://discord.com/api/v10/oauth2/token", response.status());

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
    tracing::info!("[Outgoing Request] GET https://discord.com/api/v10/users/@me");
    let response = http_client
        .get("https://discord.com/api/v10/users/@me")
        .bearer_auth(access_token)
        .send()
        .await?;
    tracing::info!("[Outgoing Response] {} for GET https://discord.com/api/v10/users/@me", response.status());

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

/// 256-bit token from a CSPRNG (uuid v4 is backed by getrandom).
pub fn generate_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// SHA-256 hex digest used to store and look up tokens. Tokens are
/// high-entropy random values, so no salt is needed.
pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// Inserts or updates the user info in PostgreSQL and returns a freshly issued
/// API token. Only the token's hash is stored; each login mints a new token
/// (multiple tokens per user stay valid, so a web login does not invalidate
/// the desktop app's saved token). Tokens idle for 180 days are pruned.
pub async fn save_or_update_user(
    db_pool: &PgPool,
    user: &DiscordUser,
) -> Result<String, AppError> {
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO users (discord_id, username, global_name, avatar, last_login) \
         VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP) \
         ON CONFLICT (discord_id) \
         DO UPDATE SET \
             username = EXCLUDED.username, \
             global_name = EXCLUDED.global_name, \
             avatar = EXCLUDED.avatar, \
             last_login = EXCLUDED.last_login \
         RETURNING id"
    )
    .bind(&user.id)
    .bind(&user.username)
    .bind(&user.global_name)
    .bind(&user.avatar)
    .fetch_one(db_pool)
    .await?;

    let token = generate_token();
    sqlx::query("INSERT INTO api_tokens (token_hash, user_id) VALUES ($1, $2)")
        .bind(hash_token(&token))
        .bind(user_id)
        .execute(db_pool)
        .await?;

    sqlx::query(
        "DELETE FROM api_tokens WHERE user_id = $1 \
         AND COALESCE(last_used_at, created_at) < NOW() - INTERVAL '180 days'"
    )
    .bind(user_id)
    .execute(db_pool)
    .await?;

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_token_is_deterministic_sha256_hex() {
        // Lookups depend on this exact encoding matching the SQL backfill:
        // encode(sha256(convert_to(token, 'UTF8')), 'hex')
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_ne!(hash_token("abc"), hash_token("abd"));
    }

    #[test]
    fn generated_tokens_are_unique_and_long() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
