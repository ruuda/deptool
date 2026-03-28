use serde::{Deserialize, Serialize};

use crate::oid::Oid;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Apply { commit: Oid },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Hello { version: String, hostname: String },
    Applied { commit: Oid },
    Error { message: String },
}
