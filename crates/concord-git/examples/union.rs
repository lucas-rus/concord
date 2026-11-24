//! Union of insertions of three versions of a file (debugging aid):
//! `cargo run -p concord-git --example union -- BASE OURS THEIRS`
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    assert_eq!(a.len(), 3, "usage: union BASE OURS THEIRS");
    let read = |p: &str| std::fs::read_to_string(p).expect("read");
    match concord_git::union_text(&read(&a[0]), &read(&a[1]), &read(&a[2])).expect("merge-file") {
        Some(m) => print!("{m}"),
        None => {
            eprintln!("not a union of insertions");
            std::process::exit(1);
        }
    }
}
