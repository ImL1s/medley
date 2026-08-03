fn main() {
    println!("cargo:rerun-if-env-changed=GROK_VERSION");
    println!("cargo:rerun-if-env-changed=MEDLEY_CHANNEL");
}
