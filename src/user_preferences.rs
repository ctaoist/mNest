use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait, IntoActiveModel, Set};
use serde::{Deserialize, Serialize};

use crate::entities::app_setting;

pub const DEFAULT_WEB_PLAYBACK_BITRATE: u32 = 0;
pub const WEB_PLAYBACK_BITRATES: &[u32] = &[0, 64, 96, 128, 192, 256, 320];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserPreferences {
    pub web_playback_bitrate: u32,
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            web_playback_bitrate: DEFAULT_WEB_PLAYBACK_BITRATE,
        }
    }
}

pub fn validate_web_playback_bitrate(value: u32) -> bool {
    WEB_PLAYBACK_BITRATES.contains(&value)
}

pub async fn load<C: ConnectionTrait>(
    db: &C,
    user_id: &str,
) -> Result<UserPreferences, sea_orm::DbErr> {
    let value = app_setting::Entity::find_by_id(web_playback_bitrate_key(user_id))
        .one(db)
        .await?
        .and_then(|setting| setting.value.parse::<u32>().ok())
        .filter(|value| validate_web_playback_bitrate(*value))
        .unwrap_or(DEFAULT_WEB_PLAYBACK_BITRATE);
    Ok(UserPreferences {
        web_playback_bitrate: value,
    })
}

pub async fn save<C: ConnectionTrait>(
    db: &C,
    user_id: &str,
    preferences: UserPreferences,
) -> Result<(), sea_orm::DbErr> {
    let key = web_playback_bitrate_key(user_id);
    if preferences.web_playback_bitrate == DEFAULT_WEB_PLAYBACK_BITRATE {
        app_setting::Entity::delete_by_id(key).exec(db).await?;
        return Ok(());
    }
    if let Some(setting) = app_setting::Entity::find_by_id(&key).one(db).await? {
        let mut active = setting.into_active_model();
        active.value = Set(preferences.web_playback_bitrate.to_string());
        active.update(db).await?;
    } else {
        app_setting::ActiveModel {
            key: Set(key),
            value: Set(preferences.web_playback_bitrate.to_string()),
        }
        .insert(db)
        .await?;
    }
    Ok(())
}

pub async fn delete<C: ConnectionTrait>(db: &C, user_id: &str) -> Result<(), sea_orm::DbErr> {
    app_setting::Entity::delete_by_id(web_playback_bitrate_key(user_id))
        .exec(db)
        .await?;
    Ok(())
}

fn web_playback_bitrate_key(user_id: &str) -> String {
    format!("user.{user_id}.web_playback_bitrate")
}
