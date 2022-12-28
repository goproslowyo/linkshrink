use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Shortlink {
    pub keyword: String,
    pub url: String,
    pub owner: String,
    #[serde(default)]
    pub hits: usize,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub description: String,
}
