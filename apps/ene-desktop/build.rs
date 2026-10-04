fn main() {
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_library_paths(std::collections::HashMap::from([(
                "lucide".into(),
                lucide_slint::lib().into(),
            )]))
            .with_style("fluent".into())
            .with_debug_info(std::env::var("PROFILE").as_deref() == Ok("debug")),
    )
    .expect("Slint UI compilation failed");
}
