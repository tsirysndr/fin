use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthResult {
    pub access_token: String,
    pub server_id: String,
    pub user: AuthUser,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthUser {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Audio,
    MusicAlbum,
    MusicArtist,
    Playlist,
    Movie,
    Series,
    Season,
    Episode,
    Video,
    Folder,
    Other,
}

impl ItemKind {
    pub fn parse(s: &str) -> Self {
        match s {
            "Audio" => Self::Audio,
            "MusicAlbum" => Self::MusicAlbum,
            "MusicArtist" => Self::MusicArtist,
            "Playlist" => Self::Playlist,
            "Movie" => Self::Movie,
            "Series" => Self::Series,
            "Season" => Self::Season,
            "Episode" => Self::Episode,
            "Video" => Self::Video,
            "Folder" | "CollectionFolder" => Self::Folder,
            _ => Self::Other,
        }
    }

    pub fn is_audio(&self) -> bool {
        matches!(self, Self::Audio | Self::MusicAlbum | Self::MusicArtist)
    }

    pub fn is_video(&self) -> bool {
        matches!(
            self,
            Self::Movie | Self::Series | Self::Season | Self::Episode | Self::Video
        )
    }

    pub fn is_playable(&self) -> bool {
        matches!(
            self,
            Self::Audio | Self::Movie | Self::Episode | Self::Video
        )
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Self::Audio => "♪",
            Self::MusicAlbum => "◈",
            Self::MusicArtist => "◊",
            Self::Playlist => "▤",
            Self::Movie => "▶",
            Self::Series => "▤",
            Self::Season => "◱",
            Self::Episode => "▶",
            Self::Video => "▶",
            Self::Folder => "▸",
            Self::Other => "•",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BaseItem {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub type_: String,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub artists: Option<Vec<String>>,
    #[serde(default)]
    pub series_name: Option<String>,
    #[serde(default)]
    pub production_year: Option<i32>,
    #[serde(default)]
    pub run_time_ticks: Option<i64>,
    #[serde(default)]
    pub media_type: Option<String>,
    /// Source container reported by Jellyfin — e.g. `"mp3"`, `"flac"`,
    /// `"mkv"`, `"mp4"`. Used to build stream URLs that match the source
    /// so no unnecessary transcoding happens.
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub index_number: Option<i32>,
    #[serde(default)]
    pub parent_index_number: Option<i32>,
    #[serde(default)]
    pub image_tags: Option<serde_json::Value>,
    #[serde(default)]
    pub is_folder: Option<bool>,
    #[serde(default)]
    pub overview: Option<String>,
}

impl BaseItem {
    pub fn kind(&self) -> ItemKind {
        ItemKind::parse(&self.type_)
    }

    pub fn duration_secs(&self) -> Option<u64> {
        self.run_time_ticks.map(|t| (t / 10_000_000) as u64)
    }

    pub fn subtitle(&self) -> String {
        match self.kind() {
            ItemKind::Audio => {
                let artists = self
                    .artists
                    .as_ref()
                    .and_then(|a| {
                        if a.is_empty() {
                            None
                        } else {
                            Some(a.join(", "))
                        }
                    })
                    .or_else(|| self.album_artist.clone())
                    .unwrap_or_default();
                let album = self.album.clone().unwrap_or_default();
                match (artists.is_empty(), album.is_empty()) {
                    (true, true) => String::new(),
                    (false, true) => artists,
                    (true, false) => album,
                    (false, false) => format!("{} — {}", artists, album),
                }
            }
            ItemKind::MusicAlbum => self
                .album_artist
                .clone()
                .or_else(|| self.artists.as_ref().map(|a| a.join(", ")))
                .unwrap_or_default(),
            ItemKind::MusicArtist => String::new(),
            ItemKind::Movie => self
                .production_year
                .map(|y| y.to_string())
                .unwrap_or_default(),
            ItemKind::Episode => match (self.parent_index_number, self.index_number) {
                (Some(s), Some(e)) => format!(
                    "S{:02}E{:02} — {}",
                    s,
                    e,
                    self.series_name.clone().unwrap_or_default()
                ),
                _ => self.series_name.clone().unwrap_or_default(),
            },
            ItemKind::Series => self
                .production_year
                .map(|y| y.to_string())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SearchResult {
    #[serde(default)]
    pub items: Vec<BaseItem>,
    #[serde(default)]
    pub total_record_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SearchHintResult {
    #[serde(default)]
    pub search_hints: Vec<SearchHint>,
    #[serde(default)]
    pub total_record_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SearchHint {
    #[serde(default, alias = "ItemId")]
    pub id: String,
    pub name: String,
    #[serde(rename = "Type", default)]
    pub type_: String,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub artists: Option<Vec<String>>,
    #[serde(default)]
    pub production_year: Option<i32>,
    #[serde(default)]
    pub run_time_ticks: Option<i64>,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub series_name: Option<String>,
}

impl SearchHint {
    pub fn into_base_item(self) -> BaseItem {
        BaseItem {
            id: self.id,
            name: self.name,
            type_: self.type_,
            album: self.album,
            album_id: None,
            album_artist: self.album_artist,
            artists: self.artists,
            series_name: self.series_name,
            production_year: self.production_year,
            run_time_ticks: self.run_time_ticks,
            media_type: self.media_type,
            container: None,
            index_number: None,
            parent_index_number: None,
            image_tags: None,
            is_folder: None,
            overview: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserView {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub collection_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserViewsResult {
    #[serde(default)]
    pub items: Vec<UserView>,
}

pub type Playlist = BaseItem;
pub type PlaylistItem = BaseItem;
