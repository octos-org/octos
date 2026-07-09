//! Check an LLM-authored raw-DOT workflow through the existing `parse_dot` path,
//! then the SAME profile gate the IR path uses — apples-to-apples with
//! `compose_check`. Usage:
//! `cargo run -p octos-pipeline --example dot_check -- <file.dot>`
//! Prints `OK\t<nodes>\t<edges>` or `FAIL\t<reason...>` (exit 1).

use std::io::Read;

use octos_pipeline::profile::{ValidationProfile, validate_under_profile};

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
    let dot = read_input();
    let graph = match octos_pipeline::parser::parse_dot(&dot) {
        Ok(g) => g,
        Err(e) => {
            println!("FAIL\tparse: {e}");
            std::process::exit(1);
        }
    };
    if let Err(cycle) = octos_pipeline::validate::detect_cycles_ignoring_marked_back_edges(&graph) {
        println!("FAIL\tcycle: {cycle}");
        std::process::exit(1);
    }
    let violations = validate_under_profile(&graph, &ValidationProfile::l2_default());
    if violations.is_empty() {
        println!("OK\t{}\t{}", graph.nodes.len(), graph.edges.len());
    } else {
        print!("FAIL");
        for v in &violations {
            match &v.node {
                Some(n) => print!("\tnode '{n}': {}", v.message),
                None => print!("\tgraph: {}", v.message),
            }
        }
        println!();
        std::process::exit(1);
    }
}
