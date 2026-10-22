fn main() {
    // Re-embed migrations when SQL files are added or changed.
    println!("cargo:rerun-if-changed=migrations");
}
