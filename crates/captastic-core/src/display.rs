use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DisplayId(pub String);

impl DisplayId {
    pub fn primary() -> Self {
        Self("primary".to_owned())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: DisplayId,
    pub name: String,
    pub bounds: Rect,
    pub scale_factor: f32,
    pub rotation_degrees: u16,
    pub is_primary: bool,
}
