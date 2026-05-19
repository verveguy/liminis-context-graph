use liminis_graph_core::Db;
use std::env;

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| "/tmp/liminis.db".to_string());
    match Db::open(&path) {
        Ok(_db) => println!("liminis-graph: opened database at {path}"),
        Err(e) => {
            eprintln!("liminis-graph: error opening {path}: {e}");
            std::process::exit(1);
        }
    }
}
