fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let path = "resources/app.rc";
    if std::path::Path::new(path).exists() {
        embed_resource::compile("resources/app.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("RC.EXE failed to compile specified resource file");
    } else {
        panic!("Resource file {} not found", path);
    }
}
