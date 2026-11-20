use crate::config::get_db_path;
use crate::error::SmError;
use crate::py::str::strip;
use crate::store::connect::connect;
use crate::store::items::{bind_text, normalize_retire_reason, retire_item_with_reason};

/// `run_retire(item_id, reason, home=...)`: the stripped id and normalized reason.
pub fn run_retire(
    item_id: &str,
    reason: &str,
    home: Option<&str>,
) -> Result<(String, String), SmError> {
    bind_text(strip(item_id))?;
    normalize_retire_reason(reason)?;
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    retire_item_with_reason(&conn, item_id, reason)
}
