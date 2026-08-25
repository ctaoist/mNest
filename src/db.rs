use argon2::{
    Argon2, PasswordHasher, PasswordVerifier,
    password_hash::{PasswordHash, SaltString},
};
use chrono::Utc;
use rand_core::OsRng;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectOptions, ConnectionTrait, Database, DatabaseConnection,
    EntityTrait, ExecResult, FromQueryResult, IntoActiveModel, QueryFilter, Set, Statement,
    sea_query::Value,
};
use uuid::Uuid;

use crate::{
    auth::{
        decrypt_server_secret, encrypt_server_secret, encrypt_subsonic_password,
        protect_subsonic_api_key,
    },
    config::{AdminSettings, DatabaseSettings},
    entities::{download_source, user},
    migrations,
};

pub struct RawStatement<'a> {
    db: &'a DatabaseConnection,
    sql: String,
    values: Vec<Value>,
}

pub fn raw(db: &DatabaseConnection, sql: impl Into<String>) -> RawStatement<'_> {
    RawStatement {
        db,
        sql: sql.into(),
        values: Vec::new(),
    }
}

impl<'a> RawStatement<'a> {
    pub fn bind(mut self, value: impl Into<Value>) -> Self {
        self.values.push(value.into());
        self
    }

    fn statement(self) -> (&'a DatabaseConnection, Statement) {
        let backend = self.db.get_database_backend();
        (
            self.db,
            Statement::from_sql_and_values(backend, self.sql, self.values),
        )
    }

    pub async fn all<T: FromQueryResult>(self) -> Result<Vec<T>, sea_orm::DbErr> {
        let (db, statement) = self.statement();
        T::find_by_statement(statement).all(db).await
    }

    pub async fn one<T: FromQueryResult>(self) -> Result<Option<T>, sea_orm::DbErr> {
        let (db, statement) = self.statement();
        T::find_by_statement(statement).one(db).await
    }

    pub async fn exec(self) -> Result<ExecResult, sea_orm::DbErr> {
        let (db, statement) = self.statement();
        db.execute(statement).await
    }
}

pub async fn connect(settings: &DatabaseSettings) -> anyhow::Result<DatabaseConnection> {
    if settings.driver == "sqlite"
        && let Some(path) = settings.url.strip_prefix("sqlite://")
    {
        let clean = path.split('?').next().unwrap_or(path);
        if clean != ":memory:"
            && let Some(parent) = std::path::Path::new(clean).parent()
        {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut options = ConnectOptions::new(settings.url.clone());
    options.max_connections(settings.max_connections);
    Ok(Database::connect(options).await?)
}

pub async fn migrate(db: &DatabaseConnection) -> anyhow::Result<()> {
    migrations::run(db).await
}

pub async fn bootstrap_admin(
    db: &DatabaseConnection,
    admin: &AdminSettings,
    secret: &str,
) -> anyhow::Result<()> {
    let existing = user::Entity::find()
        .filter(user::Column::Username.eq(&admin.username))
        .one(db)
        .await?;
    if let Some(existing) = existing.as_ref()
        && !admin.overwrite_existing
    {
        if existing.subsonic_password.is_empty()
            && PasswordHash::new(&existing.password_hash)
                .ok()
                .is_some_and(|hash| {
                    Argon2::default()
                        .verify_password(admin.password.as_bytes(), &hash)
                        .is_ok()
                })
        {
            let mut active = existing.clone().into_active_model();
            active.subsonic_password = Set(encrypt_subsonic_password(
                &admin.password,
                secret,
                &admin.username,
            )?);
            active.update(db).await?;
        }
        return Ok(());
    }

    let hash = Argon2::default()
        .hash_password(admin.password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .to_string();
    let subsonic_api_key = Uuid::new_v4().simple().to_string();
    let subsonic_password = encrypt_subsonic_password(&admin.password, secret, &admin.username)?;
    if let Some(existing) = existing {
        let protected_api_key = protect_subsonic_api_key(&subsonic_api_key, secret, &existing.id)?;
        let mut active = existing.into_active_model();
        active.password_hash = Set(hash);
        active.email = Set(admin.email.clone());
        active.role = Set("admin".into());
        active.subsonic_token = Set(protected_api_key);
        active.subsonic_password = Set(subsonic_password);
        active.update(db).await?;
    } else {
        let user_id = Uuid::new_v4().to_string();
        let protected_api_key = protect_subsonic_api_key(&subsonic_api_key, secret, &user_id)?;
        user::ActiveModel {
            id: Set(user_id),
            username: Set(admin.username.clone()),
            password_hash: Set(hash),
            email: Set(admin.email.clone()),
            role: Set("admin".into()),
            subsonic_token: Set(protected_api_key),
            subsonic_password: Set(subsonic_password),
            created_at: Set(Utc::now().to_rfc3339()),
        }
        .insert(db)
        .await?;
    }
    Ok(())
}

pub async fn protect_subsonic_api_keys(
    db: &DatabaseConnection,
    secret: &str,
) -> anyhow::Result<()> {
    for account in user::Entity::find().all(db).await? {
        if account.subsonic_token.is_empty() || account.subsonic_token.starts_with("k1:") {
            continue;
        }
        let protected = protect_subsonic_api_key(&account.subsonic_token, secret, &account.id)?;
        let mut active = account.into_active_model();
        active.subsonic_token = Set(protected);
        active.update(db).await?;
    }
    Ok(())
}

pub async fn protect_download_source_secrets(
    db: &DatabaseConnection,
    secret: &str,
) -> anyhow::Result<()> {
    for source in download_source::Entity::find().all(db).await? {
        let password_aad = format!("download-source:{}:password", source.id);
        let cookie_aad = format!("download-source:{}:cookie", source.id);
        let protected_password = protect_secret(&source.password, secret, &password_aad)?;
        let protected_cookie = protect_secret(&source.cookie, secret, &cookie_aad)?;
        if protected_password == source.password && protected_cookie == source.cookie {
            continue;
        }
        let mut active = source.into_active_model();
        active.password = Set(protected_password);
        active.cookie = Set(protected_cookie);
        active.update(db).await?;
    }
    Ok(())
}

fn protect_secret(value: &str, secret: &str, aad: &str) -> anyhow::Result<String> {
    if value.is_empty() {
        Ok(value.to_owned())
    } else if value.starts_with("v1:") {
        decrypt_server_secret(value, secret, aad)?;
        Ok(value.to_owned())
    } else {
        encrypt_server_secret(value, secret, aad)
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::PaginatorTrait;

    use super::*;
    use crate::config::{AdminSettings, DatabaseSettings};
    use crate::entities::{
        app_setting, download_source, play_queue, playback_state, schema_migration, scrobble,
        user_subsonic_access, user_track_stat,
    };

    #[tokio::test]
    async fn migrates_and_bootstraps_admin_once() {
        let db = connect(&DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        migrate(&db).await.unwrap();
        schema_migration::Entity::delete_by_id(migrations::BASELINE)
            .exec(&db)
            .await
            .unwrap();
        schema_migration::ActiveModel {
            version: Set("legacy".into()),
            applied_at: Set(Utc::now().to_rfc3339()),
        }
        .insert(&db)
        .await
        .unwrap();
        migrate(&db).await.unwrap();
        migrate(&db).await.unwrap();
        let admin = AdminSettings {
            username: "admin".into(),
            password: "a-strong-test-password".into(),
            email: "admin@example.com".into(),
            overwrite_existing: false,
        };
        bootstrap_admin(&db, &admin, "test-secret-with-at-least-32-characters")
            .await
            .unwrap();
        bootstrap_admin(&db, &admin, "test-secret-with-at-least-32-characters")
            .await
            .unwrap();
        assert_eq!(user::Entity::find().count(&db).await.unwrap(), 1);
        assert_eq!(
            schema_migration::Entity::find().count(&db).await.unwrap(),
            1
        );
        assert!(
            schema_migration::Entity::find_by_id(migrations::BASELINE)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
        let account = user::Entity::find().one(&db).await.unwrap().unwrap();
        assert!(account.subsonic_token.starts_with("k1:"));
        let api_key = crate::auth::reveal_subsonic_api_key(
            &account.subsonic_token,
            "test-secret-with-at-least-32-characters",
            &account.id,
        )
        .unwrap();
        assert!(!api_key.is_empty());
        assert!(
            crate::auth::authenticate_subsonic(
                &db,
                &std::collections::HashMap::from([("apiKey".into(), api_key)]),
                "test-secret-with-at-least-32-characters",
            )
            .await
            .unwrap()
            .is_some()
        );

        let mut active = account.into_active_model();
        active.subsonic_token = Set("legacy-plain-api-key".into());
        active.update(&db).await.unwrap();
        protect_subsonic_api_keys(&db, "test-secret-with-at-least-32-characters")
            .await
            .unwrap();
        let protected = user::Entity::find().one(&db).await.unwrap().unwrap();
        assert!(protected.subsonic_token.starts_with("k1:"));
        assert!(!protected.subsonic_token.contains("legacy-plain-api-key"));
        assert_eq!(
            crate::auth::reveal_subsonic_api_key(
                &protected.subsonic_token,
                "test-secret-with-at-least-32-characters",
                &protected.id,
            )
            .unwrap(),
            "legacy-plain-api-key"
        );
    }

    #[tokio::test]
    async fn upgrades_an_existing_baseline_with_app_settings() {
        let db = connect(&DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        migrate(&db).await.unwrap();
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "DROP TABLE app_settings",
        ))
        .await
        .unwrap();
        schema_migration::Entity::delete_by_id(migrations::BASELINE)
            .exec(&db)
            .await
            .unwrap();
        schema_migration::ActiveModel {
            version: Set("baseline".into()),
            applied_at: Set(Utc::now().to_rfc3339()),
        }
        .insert(&db)
        .await
        .unwrap();

        migrate(&db).await.unwrap();

        assert_eq!(app_setting::Entity::find().count(&db).await.unwrap(), 0);
        assert!(
            schema_migration::Entity::find_by_id(migrations::BASELINE)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            schema_migration::Entity::find_by_id("baseline")
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn upgrades_baseline_three_with_open_subsonic_compatibility_state() {
        let db = connect(&DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        migrate(&db).await.unwrap();
        db.execute_unprepared("DROP TABLE user_subsonic_access")
            .await
            .unwrap();
        db.execute_unprepared("DROP TABLE playback_states")
            .await
            .unwrap();
        db.execute_unprepared("ALTER TABLE play_queues DROP COLUMN current_index")
            .await
            .unwrap();
        schema_migration::Entity::delete_by_id(migrations::BASELINE)
            .exec(&db)
            .await
            .unwrap();
        schema_migration::ActiveModel {
            version: Set("baseline-3".into()),
            applied_at: Set(Utc::now().to_rfc3339()),
        }
        .insert(&db)
        .await
        .unwrap();

        migrate(&db).await.unwrap();

        assert_eq!(
            schema_migration::Entity::find().count(&db).await.unwrap(),
            1
        );
        assert_eq!(
            user_subsonic_access::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            0
        );
        assert_eq!(playback_state::Entity::find().count(&db).await.unwrap(), 0);
        play_queue::ActiveModel {
            user_id: Set("user".into()),
            track_ids: Set("[]".into()),
            current_id: Set(None),
            current_index: Set(Some(2)),
            position: Set(0),
            changed_at: Set(Utc::now().to_rfc3339()),
            changed_by: Set("test".into()),
        }
        .insert(&db)
        .await
        .unwrap();
        assert_eq!(
            play_queue::Entity::find_by_id("user")
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .current_index,
            Some(2)
        );
    }

    #[tokio::test]
    async fn backfills_user_track_stats_when_upgrading_the_baseline() {
        let db = connect(&DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        migrate(&db).await.unwrap();
        scrobble::ActiveModel {
            id: Set("play-1".into()),
            user_id: Set("user-1".into()),
            track_id: Set("track-1".into()),
            played_at: Set("2026-08-02T00:00:00+00:00".into()),
            submission: Set(1),
        }
        .insert(&db)
        .await
        .unwrap();
        scrobble::ActiveModel {
            id: Set("now-playing".into()),
            user_id: Set("user-1".into()),
            track_id: Set("track-1".into()),
            played_at: Set("2026-08-03T00:00:00+00:00".into()),
            submission: Set(0),
        }
        .insert(&db)
        .await
        .unwrap();
        schema_migration::Entity::delete_by_id(migrations::BASELINE)
            .exec(&db)
            .await
            .unwrap();
        schema_migration::ActiveModel {
            version: Set("baseline-2".into()),
            applied_at: Set(Utc::now().to_rfc3339()),
        }
        .insert(&db)
        .await
        .unwrap();

        migrate(&db).await.unwrap();

        let stats = user_track_stat::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.play_count, 1);
        assert_eq!(stats.last_played_at, "2026-08-02T00:00:00+00:00");
    }

    #[tokio::test]
    async fn encrypts_existing_download_source_credentials_at_startup() {
        let db = connect(&DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        migrate(&db).await.unwrap();
        let now = Utc::now().to_rfc3339();
        download_source::ActiveModel {
            id: Set("source".into()),
            kind: Set("subsonic".into()),
            name: Set("Source".into()),
            base_url: Set("https://music.example".into()),
            username: Set("user".into()),
            password: Set("plain-password".into()),
            cookie: Set("plain-cookie".into()),
            account_name: Set(String::new()),
            enabled: Set(1),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        protect_download_source_secrets(&db, "test-secret-with-at-least-32-characters")
            .await
            .unwrap();

        let source = download_source::Entity::find_by_id("source")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(source.password.starts_with("v1:"));
        assert!(source.cookie.starts_with("v1:"));
        assert_eq!(
            decrypt_server_secret(
                &source.password,
                "test-secret-with-at-least-32-characters",
                "download-source:source:password"
            )
            .unwrap(),
            "plain-password"
        );
    }
}
