use crate::domain::{AnnotationObject, SampleRepository};

pub fn get_image_annotations(project_id: &str, image_id: &str) -> Vec<AnnotationObject> {
    SampleRepository::new().image_annotations(project_id, image_id)
}

pub fn save_image_annotations(
    project_id: &str,
    image_id: &str,
    objects: Vec<AnnotationObject>,
) -> Result<(), String> {
    SampleRepository::new().save_image_annotations(project_id, image_id, objects)
}
