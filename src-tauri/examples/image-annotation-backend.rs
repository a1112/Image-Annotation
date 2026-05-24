fn main() {
    println!(
        "Image Annotation Rust backend listening on {}",
        image_annotation_lib::http_backend::backend_base_url()
    );
    if let Err(error) = image_annotation_lib::http_backend::run_foreground_backend() {
        eprintln!("backend failed: {error}");
        std::process::exit(1);
    }
}
