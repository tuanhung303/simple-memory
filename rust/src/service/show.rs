use crate::config::get_db_path;
use crate::core::item::Item;
use crate::error::SmError;
use crate::py::str::strip;
use crate::store::connect::connect;
use crate::store::items::get_item;

/// `run_show(item_id, home=...)`.
#[cfg(test)]
pub fn run_show(item_id: &str, home: Option<&str>) -> Result<Item, SmError> {
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    get_item(&conn, strip(item_id))?
        .ok_or_else(|| SmError::key(&format!("Item not found: {item_id}")))
}

/// `run_show_many(item_ids, home=...)`: (found items in the order given, ids not found); a
/// repeated id counts once.
pub fn run_show_many<S: AsRef<str>>(
    item_ids: &[S],
    home: Option<&str>,
) -> Result<(Vec<Item>, Vec<String>), SmError> {
    let conn = connect(get_db_path(home)?.as_str(), None)?;
    let mut ids: Vec<&str> = Vec::new();
    for i in item_ids
        .iter()
        .map(|i| strip(i.as_ref()))
        .filter(|i| !i.is_empty())
    {
        if !ids.contains(&i) {
            ids.push(i);
        }
    }
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    for id in ids {
        match get_item(&conn, id)? {
            Some(it) => found.push(it),
            None => missing.push(id.to_string()),
        }
    }
    Ok((found, missing))
}
