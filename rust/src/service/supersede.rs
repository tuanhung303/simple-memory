use crate::config::get_db_path;
use crate::error::SmError;
use crate::py::str::strip;
use crate::store::connect::connect;
use crate::store::items::supersede_item;

/// `run_supersede(old_id, new_id, home=...)`: the stripped ids.
pub fn run_supersede(
    old_id: &str,
    new_id: &str,
    home: Option<&str>,
) -> Result<(String, String), SmError> {
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    supersede_item(&conn, strip(old_id), strip(new_id))?;
    Ok((strip(old_id).to_string(), strip(new_id).to_string()))
}
