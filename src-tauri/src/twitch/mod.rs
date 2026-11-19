pub mod api;
pub mod auth;
pub mod chat;
pub mod storage;

use serde::{Deserialize, Serialize};

// Les deux comptes de l'application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Streamer,
    Bot,
}

impl Role {
    pub fn key(self) -> &'static str {
        match self {
            Role::Streamer => "streamer",
            Role::Bot => "bot",
        }
    }

    pub fn other(self) -> Role {
        match self {
            Role::Streamer => Role::Bot,
            Role::Bot => Role::Streamer,
        }
    }
}