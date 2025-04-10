fn main() {

    let path = "resources/app.rc";
    if std::path::Path::new(path).exists() {
        embed_resource::compile("resources/app.rc", embed_resource::NONE).manifest_optional().expect("RC.EXE failed to compile specified resource file");
    } else {
        panic!("Resource file {} not found", path);
    }

}