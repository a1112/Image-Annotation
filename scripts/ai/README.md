# Optional local AI bridges

`onnx_detection_bridge.py` and `labelme_ai_bridge.py` are copied from the user-provided `labelImg/cpp/tools` reference and embedded in the Rust executable for local Python execution. The reference project is MIT licensed; its license text is retained in `LICENSE-labelImg` in this directory. Model weights are not bundled. ONNX dependencies are listed in `requirements-onnx.txt`; OSAM dependencies are optional and reported by the bridge when missing.

Set `IMAGE_ANNOTATION_PYTHON` to choose a Python executable. AI runs only after the user requests it in the annotation workspace.
