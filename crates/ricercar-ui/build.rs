fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .with_bundled_translations(concat!(env!("CARGO_MANIFEST_DIR"), "/lang"));
    slint_build::compile_with_config("ui/app.slint", config).expect("slint build");
    println!("cargo:rerun-if-changed=lang");
}
