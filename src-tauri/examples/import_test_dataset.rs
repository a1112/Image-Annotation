use image_annotation_lib::datasets;

fn main() {
    let dataset_key = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "coco128".to_string());

    match datasets::download_test_dataset(&dataset_key) {
        Ok(job) => {
            println!("{}", job.message);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
