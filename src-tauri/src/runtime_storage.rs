//! Packaged storage selection. No Tauri dependency: these exact filesystem and
//! recovery paths can be tested with temporary directories, without user data.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Explicit standalone selection; no environment-variable or current-directory
/// fallback is interpreted as user consent to start a different workspace.
pub fn parse_data_root_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Option<PathBuf>, String> {
    let usage = "Usage: image-annotation-backend [--data-root <absolute-workspace-root>]. The root contains data/workspaces/default and data/test_data; missing directories are created. Required for release builds. No data is moved or copied.";
    let mut args = args.into_iter();
    match args.next() {
        None => Ok(None),
        Some(flag) if flag == "--data-root" => {
            let root = args
                .next()
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .ok_or_else(|| usage.to_string())?;
            if args.next().is_some() {
                return Err(usage.into());
            }
            Ok(Some(root))
        }
        _ => Err(usage.into()),
    }
}

const SELECTION_FILE: &str = "storage-root.txt";
const SELECTION_HEADER: &str = "image-annotation-storage-v1\n";
const DATA_DIRS: [&str; 3] = [
    "data/workspaces/default/projects",
    "data/test_data/projects",
    "data/test_data/cache/downloads",
];

#[derive(Debug, PartialEq, Eq)]
pub enum RootChoice {
    AppData,
    Existing(PathBuf),
    Cancel,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryChoice {
    Retry,
    ChooseAgain,
    Cancel,
}

pub trait StorageUi {
    fn choose_root(
        &mut self,
        app_data: &Path,
        legacy_root: &Path,
        legacy: &Result<bool, String>,
    ) -> RootChoice;
    fn recover(&mut self, error: &str) -> RecoveryChoice;
}

/// An existing saved selection is never replaced by an implicit fallback.
/// Only an explicit choice may create directories or change the selection.
pub fn resolve_root(
    app_data: &Path,
    legacy_root: &Path,
    ui: &mut impl StorageUi,
) -> Option<PathBuf> {
    loop {
        match read_selection(app_data)
            .and_then(|root| root.map(|root| prepare_existing(&root)).transpose())
        {
            Ok(Some(root)) => return Some(root),
            Ok(None) => break,
            Err(error) => match ui.recover(&error) {
                RecoveryChoice::Retry => continue,
                RecoveryChoice::ChooseAgain => break,
                RecoveryChoice::Cancel => return None,
            },
        }
    }

    loop {
        // Inspect errors (including PermissionDenied) remain visible to the user.
        let legacy = has_existing_data(legacy_root);
        let choice = ui.choose_root(app_data, legacy_root, &legacy);
        if choice == RootChoice::Cancel {
            return None;
        }
        loop {
            let prepared = match &choice {
                RootChoice::AppData => prepare_new(app_data),
                RootChoice::Existing(root) => prepare_existing(root),
                RootChoice::Cancel => unreachable!(),
            };
            match prepared.and_then(|root| {
                save_selection(app_data, &root)?;
                Ok(root)
            }) {
                Ok(root) => return Some(root),
                Err(error) => match ui.recover(&error) {
                    RecoveryChoice::Retry => continue,
                    RecoveryChoice::ChooseAgain => break,
                    RecoveryChoice::Cancel => return None,
                },
            }
        }
    }
}

fn access_error(action: &str, path: &Path, error: io::Error) -> String {
    format!("{action}: {} ({error}). 请检查该目录是否存在、已挂载且当前用户可读写，然后重试或重新选择已有数据目录。", path.display())
}

/// NotFound is the only absence case. Denied reads and malformed paths must not
/// masquerade as an empty installation.
pub fn has_existing_data(root: &Path) -> Result<bool, String> {
    let mut found = false;
    for suffix in ["data/workspaces/default", "data/test_data"] {
        let path = root.join(suffix);
        match fs::read_dir(&path) {
            Ok(mut entries) => {
                if let Some(entry) = entries.next() {
                    entry.map_err(|error| access_error("无法检查已有数据", &path, error))?;
                    found = true;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(access_error("无法检查已有数据", &path, error)),
        }
    }
    Ok(found)
}

pub fn prepare_existing(root: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err("数据目录必须是绝对路径，请重新选择包含 data 文件夹的工作目录。".into());
    }
    // Check before any mkdir: a removed disk/root must not be recreated empty.
    if !has_existing_data(root)? {
        return Err(format!("未找到已有数据：{}。请选择包含 data/workspaces/default 或 data/test_data 的工作目录；未创建替代目录。", root.display()));
    }
    prepare_new(root)
}

pub fn prepare_new(root: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err("数据目录必须是绝对路径。".into());
    }
    for suffix in DATA_DIRS {
        let path = root.join(suffix);
        fs::create_dir_all(&path)
            .map_err(|error| access_error("无法准备数据目录", &path, error))?;
        probe_writable(&path)?;
    }
    fs::canonicalize(root).map_err(|error| access_error("无法解析数据目录", root, error))
}

fn create_temporary_file(parent: &Path) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..100 {
        let path = parent.join(format!(
            ".image-annotation-{}-{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot create a unique temporary file",
    ))
}

fn probe_writable(parent: &Path) -> Result<(), String> {
    let (path, mut file) = create_temporary_file(parent)
        .map_err(|error| access_error("数据目录不可写", parent, error))?;
    let result = file
        .write_all(b"storage write check\n")
        .and_then(|()| file.sync_all());
    drop(file);
    let cleanup = fs::remove_file(&path);
    result
        .and(cleanup)
        .map_err(|error| access_error("数据目录写入检查失败", parent, error))
}

pub fn read_selection(app_data: &Path) -> Result<Option<PathBuf>, String> {
    let path = app_data.join(SELECTION_FILE);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(access_error("无法读取保存的数据目录", &path, error)),
    };
    let root = contents
        .strip_prefix(SELECTION_HEADER)
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .ok_or_else(|| {
            format!(
                "数据目录配置无效：{}。请选择已有数据目录；未使用空白目录替代。",
                path.display()
            )
        })?;
    Ok(Some(root))
}

pub fn save_selection(app_data: &Path, root: &Path) -> Result<(), String> {
    save_selection_with(app_data, root, |source, target| fs::rename(source, target))
}

fn save_selection_with(
    app_data: &Path,
    root: &Path,
    replace: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<(), String> {
    let root = root
        .to_str()
        .filter(|_| root.is_absolute())
        .ok_or_else(|| "无法保存数据目录：路径必须是有效的 Unicode 绝对路径。".to_string())?;
    fs::create_dir_all(app_data)
        .map_err(|error| access_error("无法保存目录选择", app_data, error))?;
    let destination = app_data.join(SELECTION_FILE);
    let (temporary, mut file) = create_temporary_file(app_data)
        .map_err(|error| access_error("无法保存目录选择", app_data, error))?;
    // Write and sync a new sibling first. A failed write/rename leaves the last
    // selection intact. Never truncate the live config or any project file.
    let result = file
        .write_all(format!("{SELECTION_HEADER}{root}").as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| replace(&temporary, &destination));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| access_error("无法保存目录选择（原配置保留）", &destination, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "image-annotation-storage-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn path(&self, suffix: &str) -> PathBuf {
            self.0.join(suffix)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct Ui {
        roots: VecDeque<RootChoice>,
        recovery: VecDeque<RecoveryChoice>,
        errors: Vec<String>,
        legacy: Vec<Result<bool, String>>,
    }
    impl StorageUi for Ui {
        fn choose_root(&mut self, _: &Path, _: &Path, legacy: &Result<bool, String>) -> RootChoice {
            self.legacy.push(legacy.clone());
            self.roots.pop_front().expect("unexpected selection prompt")
        }
        fn recover(&mut self, error: &str) -> RecoveryChoice {
            self.errors.push(error.into());
            self.recovery
                .pop_front()
                .expect("unexpected recovery prompt")
        }
    }
    fn ui(roots: Vec<RootChoice>, recovery: Vec<RecoveryChoice>) -> Ui {
        Ui {
            roots: roots.into(),
            recovery: recovery.into(),
            ..Ui::default()
        }
    }

    #[test]
    fn fresh_packaged_root_requires_explicit_choice_and_is_persisted() {
        let f = Fixture::new();
        let app = f.path("app-data");
        let legacy = f.path("missing-build-machine");
        let mut ui = ui(vec![RootChoice::AppData], vec![]);
        assert_eq!(resolve_root(&app, &legacy, &mut ui), Some(app.clone()));
        assert_eq!(read_selection(&app).unwrap(), Some(app.clone()));
        assert!(!legacy.exists());
        for suffix in DATA_DIRS {
            assert!(app.join(suffix).is_dir());
        }
    }
    #[test]
    fn cancel_does_not_create_app_data_or_a_legacy_replacement() {
        let f = Fixture::new();
        let mut ui = ui(vec![RootChoice::Cancel], vec![]);
        assert_eq!(
            resolve_root(&f.path("app"), &f.path("legacy"), &mut ui),
            None
        );
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 0);
    }
    #[test]
    fn saved_selection_reopens_without_prompt_or_path_rewriting() {
        let f = Fixture::new();
        let external = prepare_new(&f.path("old-repository")).unwrap();
        save_selection(&f.path("app"), &external).unwrap();
        let mut ui = Ui::default();
        assert_eq!(
            resolve_root(&f.path("app"), &f.path("different-builder"), &mut ui),
            Some(external)
        );
        assert!(ui.legacy.is_empty());
    }
    #[test]
    fn selected_legacy_data_and_linked_dataset_paths_are_untouched() {
        let f = Fixture::new();
        let legacy = prepare_new(&f.path("legacy")).unwrap();
        let manifest = legacy.join("data/workspaces/default/projects/project.json");
        let original =
            br#"{"sourceDatasetKey":"local-linked","rootPath":"/Volumes/Photos/selected-source"}"#;
        fs::write(&manifest, original).unwrap();
        let mut ui = ui(vec![RootChoice::Existing(legacy.clone())], vec![]);
        assert_eq!(
            resolve_root(&f.path("app"), &legacy, &mut ui),
            Some(legacy.clone())
        );
        assert_eq!(ui.legacy, vec![Ok(true)]);
        assert_eq!(fs::read(manifest).unwrap(), original);
        assert!(!f.path("app/data").exists());
    }
    #[test]
    fn explicit_new_root_does_not_migrate_or_remove_legacy_data() {
        let f = Fixture::new();
        let legacy = prepare_new(&f.path("legacy")).unwrap();
        fs::write(legacy.join("data/test_data/registry.json"), "old data").unwrap();
        let mut ui = ui(vec![RootChoice::AppData], vec![]);
        assert_eq!(
            resolve_root(&f.path("app"), &legacy, &mut ui),
            Some(f.path("app"))
        );
        assert_eq!(
            fs::read_to_string(legacy.join("data/test_data/registry.json")).unwrap(),
            "old data"
        );
        assert!(!f.path("app/data/test_data/registry.json").exists());
    }
    #[test]
    fn missing_saved_root_cannot_silently_recreate_or_fallback() {
        let f = Fixture::new();
        let missing = f.path("removed-volume");
        save_selection(&f.path("app"), &missing).unwrap();
        let mut ui = ui(vec![], vec![RecoveryChoice::Retry, RecoveryChoice::Cancel]);
        assert_eq!(
            resolve_root(&f.path("app"), &f.path("legacy"), &mut ui),
            None
        );
        assert_eq!(ui.errors.len(), 2);
        assert!(!missing.exists());
        assert!(!f.path("app/data").exists());
        assert_eq!(read_selection(&f.path("app")).unwrap(), Some(missing));
    }
    #[test]
    fn failed_saved_root_can_be_explicitly_reselected() {
        let f = Fixture::new();
        save_selection(&f.path("app"), &f.path("missing")).unwrap();
        let root = prepare_new(&f.path("real-data")).unwrap();
        let mut ui = ui(
            vec![RootChoice::Existing(root.clone())],
            vec![RecoveryChoice::ChooseAgain],
        );
        assert_eq!(
            resolve_root(&f.path("app"), &f.path("legacy"), &mut ui),
            Some(root.clone())
        );
        assert_eq!(read_selection(&f.path("app")).unwrap(), Some(root));
    }
    #[test]
    fn wrong_existing_folder_is_rejected_without_creating_data() {
        let f = Fixture::new();
        let wrong = f.path("pictures");
        fs::create_dir(&wrong).unwrap();
        let mut ui = ui(
            vec![RootChoice::Existing(wrong.clone()), RootChoice::Cancel],
            vec![RecoveryChoice::ChooseAgain],
        );
        assert_eq!(
            resolve_root(&f.path("app"), &f.path("legacy"), &mut ui),
            None
        );
        assert!(!wrong.join("data").exists());
    }
    #[test]
    fn malformed_selection_does_not_fall_back_to_new_data() {
        let f = Fixture::new();
        fs::write(f.path(SELECTION_FILE), "relative/invalid").unwrap();
        let mut ui = ui(vec![], vec![RecoveryChoice::Cancel]);
        assert_eq!(resolve_root(&f.0, &f.path("legacy"), &mut ui), None);
        assert!(!f.path("data").exists());
    }
    #[test]
    fn path_inspection_errors_are_not_treated_as_absence() {
        let f = Fixture::new();
        fs::create_dir_all(f.path("legacy/data")).unwrap();
        fs::write(f.path("legacy/data/test_data"), "not a directory").unwrap();
        assert!(has_existing_data(&f.path("legacy")).is_err());
        let mut ui = ui(vec![RootChoice::Cancel], vec![]);
        assert_eq!(
            resolve_root(&f.path("app"), &f.path("legacy"), &mut ui),
            None
        );
        assert!(ui.legacy[0].is_err());
    }
    #[test]
    fn failed_atomic_replace_preserves_previous_selection_and_cleans_temp() {
        let f = Fixture::new();
        save_selection(&f.0, &f.path("original")).unwrap();
        let error = save_selection_with(&f.0, &f.path("replacement"), |_, _| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "simulated denied rename",
            ))
        })
        .unwrap_err();
        assert!(error.contains("原配置保留"));
        assert_eq!(read_selection(&f.0).unwrap(), Some(f.path("original")));
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    }
    #[test]
    fn config_failure_is_reported_and_does_not_select_the_prepared_root() {
        let f = Fixture::new();
        let root = prepare_new(&f.path("existing")).unwrap();
        fs::create_dir_all(f.path("app/storage-root.txt")).unwrap();
        let mut ui = ui(
            vec![RootChoice::Existing(root)],
            vec![RecoveryChoice::ChooseAgain, RecoveryChoice::Cancel],
        );
        assert_eq!(
            resolve_root(&f.path("app"), &f.path("legacy"), &mut ui),
            None
        );
        assert_eq!(ui.errors.len(), 2);
        assert!(f.path("app/storage-root.txt").is_dir());
        assert_eq!(fs::read_dir(f.path("app")).unwrap().count(), 1);
    }
    #[test]
    fn write_probe_keeps_existing_files_and_removes_only_its_own_files() {
        let f = Fixture::new();
        let root = prepare_new(&f.path("root")).unwrap();
        let projects = root.join(DATA_DIRS[0]);
        fs::write(projects.join("keep"), "unchanged").unwrap();
        prepare_existing(&root).unwrap();
        assert_eq!(
            fs::read_to_string(projects.join("keep")).unwrap(),
            "unchanged"
        );
        assert_eq!(fs::read_dir(projects).unwrap().count(), 1);
    }
    #[test]
    fn cli_preserves_the_ordinary_development_launch() {
        assert_eq!(parse_data_root_args(Vec::new()), Ok(None));
    }
    #[test]
    fn cli_accepts_only_an_explicit_absolute_root() {
        let f = Fixture::new();
        let root = f.path("existing data");
        assert_eq!(
            parse_data_root_args(vec!["--data-root".into(), root.clone().into_os_string()]),
            Ok(Some(root.clone()))
        );
        assert!(!root.exists(), "parsing must not create directories");
    }
    #[test]
    fn cli_rejects_missing_relative_unknown_or_extra_arguments() {
        for args in [
            vec!["--data-root"],
            vec!["--data-root", "relative"],
            vec!["--unknown"],
            vec!["--data-root", "/absolute", "extra"],
        ] {
            assert!(parse_data_root_args(args.into_iter().map(std::ffi::OsString::from)).is_err());
        }
    }

    #[test]
    fn relative_roots_are_rejected() {
        assert!(prepare_new(Path::new("relative")).is_err());
        assert!(prepare_existing(Path::new("relative")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn denied_legacy_read_is_visible_and_never_becomes_empty() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let denied = f.path("legacy/data/test_data");
        fs::create_dir_all(&denied).unwrap();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
        let result = has_existing_data(&f.path("legacy"));
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            result.is_err(),
            "run as an unprivileged user to exercise denied access"
        );
    }
    #[cfg(unix)]
    #[test]
    fn denied_config_write_keeps_the_previous_selection() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let app = f.path("app");
        save_selection(&app, &f.path("original")).unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o500)).unwrap();
        let result = save_selection(&app, &f.path("new"));
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            result.is_err(),
            "run as an unprivileged user to exercise denied access"
        );
        assert_eq!(read_selection(&app).unwrap(), Some(f.path("original")));
        assert_eq!(fs::read_dir(app).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn denied_selected_root_does_not_fall_back() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let root = prepare_new(&f.path("root")).unwrap();
        save_selection(&f.path("app"), &root).unwrap();
        let projects = root.join(DATA_DIRS[0]);
        fs::set_permissions(&projects, fs::Permissions::from_mode(0o500)).unwrap();
        let mut ui = ui(vec![], vec![RecoveryChoice::Cancel]);
        let result = resolve_root(&f.path("app"), &f.path("legacy"), &mut ui);
        fs::set_permissions(&projects, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result, None);
        assert!(ui.errors[0].contains("不可写"));
        assert!(!f.path("app/data").exists());
    }
}
