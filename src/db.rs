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
    auth::{decrypt_server_secret, encrypt_server_secret, encrypt_subsonic_password},
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
    let subsonic_token = Uuid::new_v4().simple().to_string();
    let subsonic_password = encrypt_subsonic_password(&admin.password, secret, &admin.username)?;
    if let Some(existing) = existing {
        let mut active = existing.into_active_model();
        active.password_hash = Set(hash);
        active.email = Set(admin.email.clone());
        active.role = Set("admin".into());
        active.subsonic_token = Set(subsonic_token);
        active.subsonic_password = Set(subsonic_password);
        active.update(db).await?;
    } else {
        user::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            username: Set(admin.username.clone()),
            password_hash: Set(hash),
            email: Set(admin.email.clone()),
            role: Set("admin".into()),
            subsonic_token: Set(subsonic_token),
            subsonic_password: Set(subsonic_password),
            created_at: Set(Utc::now().to_rfc3339()),
        }
        .insert(db)
        .await?;
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
    use crate::entities::{app_setting, download_source, schema_migration};

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
