use std::sync::{Arc, RwLock};

use crate::Client;

pub fn move_clients(
    src: &Arc<RwLock<Vec<Client>>>,
    predicate: impl Fn(&Client) -> bool,
) -> Arc<RwLock<Vec<Client>>> {
    let mut src_vec = src.write().unwrap();
    let mut moved = Vec::new();

    let mut i = src_vec.len();
    while i > 0 {
        i -= 1;
        if predicate(&src_vec[i]) {
            let client = src_vec.swap_remove(i); // MOVE out
            moved.push(client); // now in destination
        }
    }

    Arc::new(RwLock::new(moved))
}
