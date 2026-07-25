use std::collections::HashSet;

use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{db, entities::artist};

pub const UNKNOWN_ARTIST: &str = "Unknown Artist";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtistCredit {
    pub id: String,
    pub name: String,
}

pub fn parse_artist_names(value: &str) -> Vec<String> {
    let mut fragments = Vec::new();
    let mut start = 0;
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(byte, b',' | b';' | b'&') && bytes.get(index + 1) == Some(&b' ') {
            fragments.push(&value[start..index]);
            start = index + 2;
        }
    }
    fragments.push(&value[start..]);

    let mut seen = HashSet::new();
    let names = fragments
        .into_iter()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| seen.insert(name.to_lowercase()))
        .map(str::to_owned)
        .collect::<Vec<_>>();

    if names.is_empty() {
        vec![UNKNOWN_ARTIST.to_owned()]
    } else {
        names
    }
}

pub fn normalize_artist_metadata(value: &str) -> String {
    if value.trim().is_empty() {
        String::new()
    } else {
        parse_artist_names(value).join("; ")
    }
}

pub async fn resolve_artist_credits(
    database: &DatabaseConnection,
    names: &[String],
) -> anyhow::Result<Vec<ArtistCredit>> {
    let mut credits = Vec::with_capacity(names.len());
    for name in names {
        let id = if let Some(existing) = artist::Entity::find()
            .filter(artist::Column::Name.eq(name))
            .one(database)
            .await?
        {
            existing.id
        } else {
            let id = Uuid::new_v4().to_string();
            artist::ActiveModel {
                id: Set(id.clone()),
                name: Set(name.clone()),
                sort_name: Set(name.to_lowercase()),
                cover_path: Set(None),
                album_count: Set(0),
                song_count: Set(0),
            }
            .insert(database)
            .await?;
            id
        };
        credits.push(ArtistCredit {
            id,
            name: name.clone(),
        });
    }
    Ok(credits)
}

pub async fn replace_track_artists(
    database: &DatabaseConnection,
    track_id: &str,
    credits: &[ArtistCredit],
) -> anyhow::Result<()> {
    db::raw(database, "DELETE FROM track_artists WHERE track_id=$1")
        .bind(track_id)
        .exec()
        .await?;
    for (position, credit) in credits.iter().enumerate() {
        db::raw(
            database,
            "INSERT INTO track_artists(track_id,artist_id,position) VALUES ($1,$2,$3)",
        )
        .bind(track_id)
        .bind(&credit.id)
        .bind(position as i64)
        .exec()
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_only_configured_artist_separators() {
        assert_eq!(
            parse_artist_names("Artist A, Artist B; Artist C & Artist D"),
            ["Artist A", "Artist B", "Artist C", "Artist D"]
        );
        assert_eq!(parse_artist_names("AC/DC、Guest"), ["AC/DC、Guest"]);
        assert_eq!(
            parse_artist_names("Artist A,Artist B;Artist C&Artist D"),
            ["Artist A,Artist B;Artist C&Artist D"]
        );
    }

    #[test]
    fn removes_empty_and_duplicate_artist_names() {
        assert_eq!(parse_artist_names("Artist A & & artist a"), ["Artist A"]);
        assert_eq!(parse_artist_names(", ; & "), [UNKNOWN_ARTIST]);
        assert_eq!(
            normalize_artist_metadata("Artist A, Artist B & Artist C"),
            "Artist A; Artist B; Artist C"
        );
    }
}
