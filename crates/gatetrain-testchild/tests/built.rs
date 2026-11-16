//! Naming the binary here makes `cargo test --workspace` build it, so the other crates' tests find it next to the gatetrain binary.
#[test]
fn the_child_program_runs() {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_gt-testchild")).args(["print", "RAN"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "RAN");
}
