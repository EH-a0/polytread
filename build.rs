fn main() {
    println!("cargo:rerun-if-changed=web/dashboard.html");

    // Async operations in the setup wizard are boxed at their polling boundary.
    // Keep a larger Windows main-thread reserve as defense in depth for the
    // cryptography and networking dependency graph used by first-time setup.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bin=polytread=/STACK:8388608");
    }
}
