//! `mailwise open <message-id>` -- resolve to a configured client and
//! launch its preferred preview path.

use anyhow::Result;

use crate::{clients, db};

pub fn run(message_id: &str) -> Result<()> {
    let db_path = db::default_db_path()?;
    let conn = db::initialize(&db_path)?;
    clients::open_message(&conn, message_id)
}
