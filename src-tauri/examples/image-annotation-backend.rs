fn main() {
    let data_root = match image_annotation_lib::parse_data_root_args(std::env::args_os().skip(1)) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    println!(
        "Starting Image Annotation Rust backend at {}",
        image_annotation_lib::http_backend::backend_base_url()
    );
    if let Err(error) = image_annotation_lib::http_backend::run_foreground_backend_at(data_root) {
        eprintln!("backend failed: {error}");
        std::process::exit(1);
    }
}
