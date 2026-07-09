//! Check an LLM-authored typed-IR workflow through the real `compose_l2` path.
//! Usage: `cargo run -p octos-pipeline --example compose_check -- <file.json>`
//! Prints `OK\t<nodes>\t<edges>` or `FAIL\t<feedback lines...>` (exit 1).

use std::io::Read;

fn read_input() -> String {
    if let Some(path) = std::env::args().nth(1) {
        std::fs::read_to_string(path).expect("read file")
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).expect("read stdin");
        s
    }
}

fn main() {
    let json = read_input();
    match octos_pipeline::compose::compose_l2(&json) {
        Ok(g) => println!("OK\t{}\t{}", g.nodes.len(), g.edges.len()),
        Err(e) => {
            print!("FAIL");
            for line in e.feedback_lines() {
                print!("\t{line}");
            }
            println!();
            std::process::exit(1);
        }
    }
}
