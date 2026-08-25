use std::collections::HashMap;

use argon2::{
    Argon2, PasswordHasher, PasswordVerifier,
    password_hash::{PasswordHash, SaltString},
};
use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, StatusCode, header, request::Parts},
};
use base64::Engine;
use chrono::{Duration, Utc};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use md5::{Digest, Md5};
use rand_core::OsRng;
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    digest, hmac,
    rand::{SecureRandom, SystemRandom},
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter, Set,
};
use serde::{Deserialize, Serialize};

use crate::{entities::user, models::User, state::AppState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub username: String,
    pub role: String,
    pub kind: String,
    pub exp: usize,
}

#[derive(Debug, Clone)]
pub struct AuthUser(pub User);

#[derive(Debug, Clone)]
pub struct AdminUser(pub User);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        web_user_from_headers(&state.db, &parts.headers, &state.settings.auth.jwt_secret)
            .await
            .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid or expired token".into()))?
            .map(Self)
            .ok_or((
                StatusCode::UNAUTHORIZED,
                "missing authorization token".into(),
            ))
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let AuthUser(user) = AuthUser::from_request_parts(parts, state).await?;
        if user.role != "admin" {
            return Err((StatusCode::FORBIDDEN, "administrator role required".into()));
        }
        Ok(Self(user))
    }
}

pub async fn web_user_from_headers(
    db: &DatabaseConnection,
    headers: &HeaderMap,
    secret: &str,
) -> anyhow::Result<Option<User>> {
    let token = bearer_token(headers).or_else(|| cookie_value(headers, "mNest_access"));
    let Some(token) = token else {
        return Ok(None);
    };
    let claims = decode_token(token, secret)?;
    if claims.kind != "access" {
        return Ok(None);
    }
    user_by_id(db, &claims.sub).await
}

pub fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("JWT "))
        .or_else(|| value.strip_prefix("jwt "))
}

pub async fn authenticate_password(
    db: &DatabaseConnection,
    username: &str,
    password: &str,
) -> anyhow::Result<Option<User>> {
    let Some(user) = user_by_name(db, username).await? else {
        let _ =
            Argon2::default().hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng));
        return Ok(None);
    };
    let parsed = PasswordHash::new(&user.password_hash)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    if Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
    {
        Ok(Some(user))
    } else {
        Ok(None)
    }
}

pub async fn authenticate_password_with_subsonic(
    db: &DatabaseConnection,
    username: &str,
    password: &str,
    secret: &str,
) -> anyhow::Result<Option<User>> {
    let Some(user) = authenticate_password(db, username, password).await? else {
        return Ok(None);
    };
    Ok(Some(
        store_subsonic_password(db, user, password, secret).await?,
    ))
}

pub fn issue_tokens(
    user: &User,
    secret: &str,
    access_minutes: i64,
    refresh_days: i64,
) -> anyhow::Result<(String, String)> {
    let access = encode_claims(user, secret, "access", Duration::minutes(access_minutes))?;
    let refresh = encode_claims(user, secret, "refresh", Duration::days(refresh_days))?;
    Ok((access, refresh))
}

fn encode_claims(
    user: &User,
    secret: &str,
    kind: &str,
    duration: Duration,
) -> anyhow::Result<String> {
    let claims = Claims {
        sub: user.id.clone(),
        username: user.username.clone(),
        role: user.role.clone(),
        kind: kind.into(),
        exp: (Utc::now() + duration).timestamp() as usize,
    };
    Ok(encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

pub fn decode_token(token: &str, secret: &str) -> anyhow::Result<Claims> {
    Ok(decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )?
    .claims)
}

pub async fn user_by_name(db: &DatabaseConnection, username: &str) -> anyhow::Result<Option<User>> {
    Ok(user::Entity::find()
        .filter(user::Column::Username.eq(username))
        .one(db)
        .await?)
}

pub async fn user_by_id(db: &DatabaseConnection, id: &str) -> anyhow::Result<Option<User>> {
    Ok(user::Entity::find_by_id(id).one(db).await?)
}

pub async fn authenticate_subsonic(
    db: &DatabaseConnection,
    params: &HashMap<String, String>,
    secret: &str,
) -> anyhow::Result<Option<User>> {
    if let Some(api_key) = params.get("apiKey") {
        if api_key.is_empty() {
            return Ok(None);
        }
        let lookup = subsonic_api_key_lookup(api_key, secret);
        let protected_prefix = format!("k1:{lookup}:");
        let candidates = user::Entity::find()
            .filter(
                Condition::any()
                    .add(user::Column::SubsonicToken.eq(api_key))
                    .add(user::Column::SubsonicToken.starts_with(&protected_prefix)),
            )
            .all(db)
            .await?;
        for candidate in candidates {
            let Ok(stored_key) =
                reveal_subsonic_api_key(&candidate.subsonic_token, secret, &candidate.id)
            else {
                continue;
            };
            if constant_time_eq(stored_key.as_bytes(), api_key.as_bytes()) {
                return Ok(Some(candidate));
            }
        }
        return Ok(None);
    }
    let Some(username) = params.get("u") else {
        return Ok(None);
    };
    let Some(user) = user_by_name(db, username).await? else {
        return Ok(None);
    };

    if let Some(password) = params.get("p") {
        let decoded;
        let password = if let Some(hex_value) = password.strip_prefix("enc:") {
            decoded = String::from_utf8(hex::decode(hex_value)?)?;
            &decoded
        } else {
            password
        };
        return authenticate_password_with_subsonic(db, username, password, secret).await;
    }
    if let (Some(salt), Some(token)) = (params.get("s"), params.get("t")) {
        let token = token.to_ascii_lowercase();
        let password_matches =
            decrypt_subsonic_password(&user.subsonic_password, secret, &user.username)
                .ok()
                .map(|password| {
                    let expected = subsonic_token(&password, salt);
                    constant_time_eq(expected.as_bytes(), token.as_bytes())
                })
                .unwrap_or(false);
        return Ok(password_matches.then_some(user));
    }
    Ok(None)
}

pub fn protect_subsonic_api_key(
    api_key: &str,
    secret: &str,
    user_id: &str,
) -> anyhow::Result<String> {
    if api_key.is_empty() {
        return Ok(String::new());
    }
    let lookup = subsonic_api_key_lookup(api_key, secret);
    let encrypted = encrypt_server_secret(api_key, secret, &format!("subsonic-api-key:{user_id}"))?;
    Ok(format!("k1:{lookup}:{encrypted}"))
}

pub fn reveal_subsonic_api_key(
    stored: &str,
    secret: &str,
    user_id: &str,
) -> anyhow::Result<String> {
    if stored.is_empty() {
        return Ok(String::new());
    }
    let Some(protected) = stored.strip_prefix("k1:") else {
        return Ok(stored.to_owned());
    };
    let (lookup, encrypted) = protected
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid protected Subsonic API key"))?;
    let api_key = decrypt_server_secret(encrypted, secret, &format!("subsonic-api-key:{user_id}"))?;
    let expected = subsonic_api_key_lookup(&api_key, secret);
    anyhow::ensure!(
        constant_time_eq(lookup.as_bytes(), expected.as_bytes()),
        "invalid protected Subsonic API key lookup"
    );
    Ok(api_key)
}

fn subsonic_api_key_lookup(api_key: &str, secret: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let mut message = b"mnest-subsonic-api-key\0".to_vec();
    message.extend_from_slice(api_key.as_bytes());
    hex::encode(hmac::sign(&key, &message).as_ref())
}

fn subsonic_token(password: &str, salt: &str) -> String {
    hex::encode(Md5::digest(format!("{password}{salt}").as_bytes()))
}

async fn store_subsonic_password(
    db: &DatabaseConnection,
    user: User,
    password: &str,
    secret: &str,
) -> anyhow::Result<User> {
    if decrypt_subsonic_password(&user.subsonic_password, secret, &user.username)
        .is_ok_and(|stored| stored == password)
    {
        return Ok(user);
    }
    let encrypted = encrypt_subsonic_password(password, secret, &user.username)?;
    let mut active = user.into_active_model();
    active.subsonic_password = Set(encrypted);
    Ok(active.update(db).await?)
}

pub fn encrypt_subsonic_password(
    password: &str,
    secret: &str,
    username: &str,
) -> anyhow::Result<String> {
    encrypt_server_secret(password, secret, username)
}

pub fn encrypt_server_secret(value: &str, secret: &str, aad: &str) -> anyhow::Result<String> {
    let key = subsonic_password_key(secret)?;
    let mut nonce_bytes = [0u8; 12];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate Subsonic credential nonce"))?;
    let mut encrypted = value.as_bytes().to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce_bytes),
        Aad::from(aad.as_bytes()),
        &mut encrypted,
    )
    .map_err(|_| anyhow::anyhow!("failed to encrypt Subsonic credential"))?;
    let mut payload = nonce_bytes.to_vec();
    payload.extend(encrypted);
    Ok(format!(
        "v1:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    ))
}

pub fn decrypt_subsonic_password(
    encrypted: &str,
    secret: &str,
    username: &str,
) -> anyhow::Result<String> {
    decrypt_server_secret(encrypted, secret, username)
}

pub fn decrypt_server_secret(encrypted: &str, secret: &str, aad: &str) -> anyhow::Result<String> {
    let encoded = encrypted
        .strip_prefix("v1:")
        .ok_or_else(|| anyhow::anyhow!("unsupported Subsonic credential format"))?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded)?;
    if payload.len() < 12 + aead::MAX_TAG_LEN {
        anyhow::bail!("invalid Subsonic credential");
    }
    let nonce_bytes: [u8; 12] = payload[..12]
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid Subsonic credential nonce"))?;
    let mut ciphertext = payload[12..].to_vec();
    let plain = subsonic_password_key(secret)?
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(aad.as_bytes()),
            &mut ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("failed to decrypt Subsonic credential"))?;
    Ok(String::from_utf8(plain.to_vec())?)
}

fn subsonic_password_key(secret: &str) -> anyhow::Result<LessSafeKey> {
    let mut material = b"mp3tag-subsonic-password\0".to_vec();
    material.extend_from_slice(secret.as_bytes());
    let digest = digest::digest(&digest::SHA256, &material);
    let key = UnboundKey::new(&aead::AES_256_GCM, digest.as_ref())
        .map_err(|_| anyhow::anyhow!("failed to derive Subsonic credential key"))?;
    Ok(LessSafeKey::new(key))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::http::{HeaderMap, HeaderValue, header};

    use super::*;
    use crate::config::{AdminSettings, DatabaseSettings};

    const SECRET: &str = "test-secret-with-at-least-32-characters";

    #[test]
    fn reads_named_cookie_without_matching_prefixes() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; mNest_access=token-value; suffix=2"),
        );
        assert_eq!(cookie_value(&headers, "mNest_access"), Some("token-value"));
        assert_eq!(cookie_value(&headers, "access"), None);
    }

    #[tokio::test]
    async fn authenticates_standard_subsonic_salted_password_token() {
        let db = crate::db::connect(&DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        crate::db::migrate(&db).await.unwrap();
        let admin = AdminSettings {
            username: "feishin".into(),
            password: "feishin-test-password".into(),
            email: String::new(),
            overwrite_existing: false,
        };
        crate::db::bootstrap_admin(&db, &admin, SECRET)
            .await
            .unwrap();

        let stored = user_by_name(&db, "feishin").await.unwrap().unwrap();
        assert_ne!(stored.subsonic_password, admin.password);
        assert_eq!(
            decrypt_subsonic_password(&stored.subsonic_password, SECRET, "feishin").unwrap(),
            admin.password
        );

        let salt = "0123456789abcdef";
        let mut params = HashMap::from([
            ("u".into(), "feishin".into()),
            ("s".into(), salt.into()),
            ("t".into(), subsonic_token(&admin.password, salt)),
        ]);
        let authenticated = authenticate_subsonic(&db, &params, SECRET)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(authenticated.username, "feishin");

        params.insert("t".into(), subsonic_token("wrong-password", salt));
        assert!(
            authenticate_subsonic(&db, &params, SECRET)
                .await
                .unwrap()
                .is_none()
        );
    }
}
