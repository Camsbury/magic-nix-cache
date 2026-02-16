fn main() {
    // Some of our static dependencies (notably the musl-cross `xz`/`liblzma`)
    // reference `__sched_cpucount`, which is a glibc symbol and isn't provided
    // by musl.
    //
    // When building a fully static musl executable, provide a tiny shim
    // implementation so linking succeeds.
    let target = std::env::var("TARGET").expect("TARGET env var is set by Cargo");

    if target.ends_with("-unknown-linux-musl") {
        println!("cargo:rerun-if-changed=build/sched_cpucount_shim.c");

        cc::Build::new()
            .file("build/sched_cpucount_shim.c")
            .compile("sched_cpucount_shim");
    }
}
