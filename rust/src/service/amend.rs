use crate::config::get_db_path;
use crate::error::SmError;
use crate::py::str::strip;
use crate::store::connect::connect;
use crate::store::items::{AmendPatch, amend_item, bind_text};

/// `run_amend(item_id, patch, home=...)`: `(new_id, body_changed)`.
pub fn run_amend(
    item_id: &str,
    patch: &AmendPatch<'_>,
    home: Option<&str>,
) -> Result<(String, bool), SmError> {
    if patch.is_empty() {
        return Err(SmError::value("amend needs at least one field to change"));
    }
    bind_text(strip(item_id))?;
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    amend_item(&conn, item_id, patch)
}
