fn main() {
    // Release builds embed `static/`; rebuild when a file there is added or changed.
    println!("cargo:rerun-if-changed=static");
}
