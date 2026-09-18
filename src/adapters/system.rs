//! The two smallest adapters: the machine's clock and its id generator.

use chrono::{DateTime, Utc};

use crate::ports::clock::Clock;
use crate::ports::ids::Ids;

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub struct UuidIds;

impl Ids for UuidIds {
    fn token_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn upload_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}
