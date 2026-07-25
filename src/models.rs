use serde::{Deserialize, Serialize};

pub use crate::entities::{
    album::Model as Album, artist::Model as Artist, job::Model as Job,
    music_folder::Model as MusicFolder, track::Model as Track, user::Model as User,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub result: bool,
    pub code: String,
    pub data: T,
    pub message: String,
}

impl<T> ApiResponse<T> {
    pub fn success(data: T) -> Self {
        Self {
            result: true,
            code: "200".into(),
            data,
            message: "success".into(),
        }
    }
    pub fn failure(data: T, message: impl Into<String>) -> Self {
        Self {
            result: false,
            code: "400".into(),
            data,
            message: message.into(),
        }
    }
}
