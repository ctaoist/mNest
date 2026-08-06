use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

macro_rules! empty_relation {
    () => {
        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    };
}

pub mod user {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "users")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub username: String,
        #[serde(skip_serializing)]
        pub password_hash: String,
        pub email: String,
        pub role: String,
        #[serde(skip_serializing)]
        pub subsonic_token: String,
        #[serde(skip_serializing)]
        pub subsonic_password: String,
        pub created_at: String,
    }

    empty_relation!();
}

pub mod music_folder {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "music_folders")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub name: String,
        pub path: String,
        pub enabled: i64,
    }

    empty_relation!();
}

pub mod artist {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "artists")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub name: String,
        pub sort_name: String,
        pub cover_path: Option<String>,
        pub album_count: i64,
        pub song_count: i64,
    }

    empty_relation!();
}

pub mod album {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "albums")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub name: String,
        pub artist_id: String,
        pub artist_name: String,
        pub year: i64,
        pub genre: String,
        pub cover_path: Option<String>,
        pub song_count: i64,
        pub duration: f64,
        pub created_at: String,
    }

    empty_relation!();
}

pub mod track {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "tracks")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub folder_id: String,
        pub path: String,
        pub relative_path: String,
        pub title: String,
        pub artist_id: String,
        pub artist_name: String,
        pub artists_json: String,
        pub album_id: Option<String>,
        pub album_name: String,
        pub album_artist: String,
        pub genre: String,
        pub year: i64,
        pub track_number: i64,
        pub disc_number: i64,
        pub duration: f64,
        pub bit_rate: i64,
        pub size: i64,
        pub suffix: String,
        pub mimetype: String,
        pub lyrics: String,
        pub comment: String,
        pub cover_path: Option<String>,
        pub mtime: i64,
        #[serde(skip_serializing)]
        pub fingerprint: String,
        pub play_count: i64,
        pub needs_scrape: i64,
        pub created_at: String,
        pub updated_at: String,
    }

    empty_relation!();
}

pub mod track_artist {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "track_artists")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub track_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub artist_id: String,
        pub position: i64,
    }

    empty_relation!();
}

pub mod download_source {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "download_sources")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub kind: String,
        pub name: String,
        pub base_url: String,
        pub username: String,
        #[serde(skip_serializing)]
        pub password: String,
        #[serde(skip_serializing)]
        pub cookie: String,
        pub account_name: String,
        pub enabled: i64,
        pub created_at: String,
        pub updated_at: String,
    }

    empty_relation!();
}

pub mod app_setting {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "app_settings")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub key: String,
        pub value: String,
    }

    empty_relation!();
}

pub mod job {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "jobs")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub kind: String,
        pub state: String,
        #[serde(skip_serializing)]
        pub payload: String,
        pub progress: f64,
        pub message: String,
        pub attempts: i64,
        #[serde(skip_serializing)]
        pub lease_until: Option<String>,
        pub created_at: String,
        pub updated_at: String,
    }

    empty_relation!();
}

pub mod favorite {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "favorites")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub item_type: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub item_id: String,
        pub created_at: String,
    }

    empty_relation!();
}

pub mod rating {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "ratings")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub item_type: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub item_id: String,
        pub rating: i64,
    }

    empty_relation!();
}

pub mod playlist {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "playlists")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub user_id: String,
        pub name: String,
        pub comment: String,
        pub public: i64,
        pub created_at: String,
        pub updated_at: String,
    }

    empty_relation!();
}

pub mod playlist_track {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "playlist_tracks")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub playlist_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub position: i64,
        pub track_id: String,
    }

    empty_relation!();
}

pub mod bookmark {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bookmarks")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub track_id: String,
        pub position: i64,
        pub comment: String,
        pub changed_at: String,
    }

    empty_relation!();
}

pub mod play_queue {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "play_queues")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: String,
        pub track_ids: String,
        pub current_id: Option<String>,
        pub position: i64,
        pub changed_at: String,
        pub changed_by: String,
    }

    empty_relation!();
}

pub mod share {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "shares")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub user_id: String,
        pub item_ids: String,
        pub description: String,
        pub expires_at: Option<String>,
        pub created_at: String,
        pub play_count: i64,
        pub last_visited_at: Option<String>,
    }

    empty_relation!();
}

pub mod internet_radio_station {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "internet_radio_stations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub name: String,
        pub stream_url: String,
        pub home_page_url: String,
    }

    empty_relation!();
}

pub mod scrobble {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "scrobbles")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub user_id: String,
        pub track_id: String,
        pub played_at: String,
        pub submission: i64,
    }

    empty_relation!();
}

pub mod user_track_stat {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "user_track_stats")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub track_id: String,
        pub play_count: i64,
        pub last_played_at: String,
    }

    empty_relation!();
}

pub mod schema_migration {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "schema_migrations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub version: String,
        pub applied_at: String,
    }

    empty_relation!();
}
