// crates/audiolib-update/src/deploy.rs
//! Owned staging and rollback; no update operation recursively deletes installations.
use crate::{Product, package};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

#[cfg(test)]
mod server_pair_tests;

/// Verified staged payload. Downloading has no effect until handoff is requested.
pub struct PreparedUpdate {
    stage: tempfile::TempDir,
    install: PathBuf,
    product: Product,
    installation_lock: File,
}

#[derive(Serialize, Deserialize)]
struct Plan {
    install: PathBuf,
    product: Product,
    executable: String,
    arguments: Vec<OsString>,
    working_directory: PathBuf,
    files: Vec<PlannedFile>,
}

#[derive(Serialize, Deserialize)]
struct PlannedFile {
    name: String,
    hash: String,
}

impl PreparedUpdate {
    pub(crate) fn stage_root(&self) -> &Path {
        self.stage.path()
    }
    pub(crate) fn new(
        stage: tempfile::TempDir,
        directory: &Path,
        product: Product,
    ) -> Result<Self> {
        let install = directory.canonicalize()?;
        let lock_path = install.join(".audiolib-update.lock");
        validate_unlinked(&lock_path)?;
        let installation_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        installation_lock
            .try_lock()
            .context("Another update is already staged for this installation")?;
        Ok(Self {
            stage,
            install,
            product,
            installation_lock,
        })
    }

    fn plan(&self, arguments: &[OsString]) -> Result<Plan> {
        let executable = std::env::current_exe()?.canonicalize()?;
        ensure!(
            executable.parent() == Some(self.install.as_path()),
            "Only installed applications can self-update"
        );
        let executable = executable
            .file_name()
            .and_then(|name| name.to_str())
            .context("Invalid executable name")?
            .to_owned();
        ensure!(
            package::required(self.product).contains(&executable.as_str()),
            "Unsupported update executable"
        );
        let mut files = Vec::new();
        collect(
            &self.stage.path().join("payload"),
            &self.stage.path().join("payload"),
            self.product,
            &mut files,
        )?;
        files.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Plan {
            install: self.install.clone(),
            product: self.product,
            executable,
            arguments: arguments.to_vec(),
            working_directory: std::env::current_dir()?,
            files,
        })
    }

    /// Spawns a private copy of the current executable as an update helper. It
    /// waits for this process to exit gracefully (120 seconds), deploys with
    /// rollback, and restarts the same application with its original arguments.
    /// The caller must now close normally; no process is forcibly terminated.
    pub fn spawn_after_exit(self, arguments: &[OsString]) -> Result<()> {
        let plan = self.plan(arguments)?;
        let plan_path = self.stage.path().join("plan.json");
        std::fs::write(&plan_path, serde_json::to_vec(&plan)?)?;
        let helper = self.stage.path().join(if cfg!(windows) {
            "update-helper.exe"
        } else {
            "update-helper"
        });
        std::fs::copy(std::env::current_exe()?, &helper)?;
        let mut command = Command::new(&helper);
        command
            .arg("--audiolib-apply-update")
            .arg(&plan_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW: helper has no interactive UI.
        }
        command.spawn().context("Start update helper")?;
        // Keep both the stage and OS lock alive until process teardown, including
        // detached background tasks. This is an intentional process-lifetime handle;
        // the OS releases it on exit. A helper timeout leaves all installed files intact.
        let _ = self.stage.keep();
        std::mem::forget(self.installation_lock);
        Ok(())
    }

    /// Call only after all application work, scans and writes have drained.
    /// Unix updates and execs in the same PID so launchd/systemd retain ownership;
    /// Windows schedules the exit-waiting helper and the caller must then return.
    pub fn deploy_and_restart(self, arguments: &[OsString]) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let plan = self.plan(arguments)?;
            let stage = self.stage.path();
            let changed = match deploy(&plan, stage, None) {
                Ok(changed) => changed,
                Err(error) if error.is::<RollbackIncomplete>() => {
                    let retained = self.stage.keep();
                    return Err(error.context(format!(
                        "Recovery backup retained at {}",
                        retained.display()
                    )));
                }
                Err(error) => return Err(error),
            };
            if restart_action(
                cfg!(target_os = "macos"),
                std::env::var_os("AUDIOLIB_LAUNCHD_LABEL"),
                std::env::var_os("XPC_SERVICE_NAME"),
            ) == RestartAction::ExitForLaunchd
            {
                // Running under launchd: exit so KeepAlive relaunches through the
                // configured launchd -> audiolib-launcher -> run.sh chain. Exec'ing the
                // binary directly bypasses the launcher and triggers macOS TCC prompts.
                let _ = self.stage.keep();
                std::process::exit(LAUNCHD_RESTART_EXIT_CODE);
            }
            let error = restart_command(&plan).exec();
            if let Err(rollback_error) = rollback(&plan, stage, &changed) {
                let retained = self.stage.keep();
                return Err(rollback_error.context(format!(
                    "Restart failed ({error}); recovery backup retained at {}",
                    retained.display()
                )));
            }
            Err(error).context("Restart updated application; previous installation restored")
        }
        #[cfg(not(unix))]
        self.spawn_after_exit(arguments)
    }
}

/// Exit code used after installing an update under launchd. Nonzero (EX_TEMPFAIL)
/// so both `KeepAlive = true` and `KeepAlive = { SuccessfulExit = false }` relaunch.
#[cfg(any(unix, test))]
pub const LAUNCHD_RESTART_EXIT_CODE: i32 = 75;

#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartAction {
    /// Replace the current process image with the new binary (same PID).
    Exec,
    /// Exit and let launchd's KeepAlive restart the service via its own chain.
    ExitForLaunchd,
}

/// Pure decision: exit for launchd only on macOS when the plist-provided
/// `AUDIOLIB_LAUNCHD_LABEL` is non-empty and equals `XPC_SERVICE_NAME` exactly.
/// `XPC_SERVICE_NAME` alone is unreliable (it is inherited, and is "0" in many
/// non-launchd shells), and the parent pid is the launcher rather than 1.
#[cfg(any(unix, test))]
fn restart_action(
    is_macos: bool,
    launchd_label: Option<OsString>,
    xpc_service_name: Option<OsString>,
) -> RestartAction {
    match (launchd_label, xpc_service_name) {
        (Some(label), Some(xpc)) if is_macos && !label.is_empty() && label == xpc => {
            RestartAction::ExitForLaunchd
        }
        _ => RestartAction::Exec,
    }
}

#[cfg(test)]
mod restart_action_tests {
    use super::*;

    const LABEL: &str = "io.github.audiolib.server";

    fn some(value: &str) -> Option<OsString> {
        Some(value.into())
    }

    #[test]
    fn macos_matching_label_exits() {
        assert_eq!(
            restart_action(true, some(LABEL), some(LABEL)),
            RestartAction::ExitForLaunchd
        );
        assert_ne!(LAUNCHD_RESTART_EXIT_CODE, 0);
    }

    #[test]
    fn unset_execs() {
        assert_eq!(restart_action(true, None, None), RestartAction::Exec);
        assert_eq!(restart_action(true, None, some(LABEL)), RestartAction::Exec);
        assert_eq!(restart_action(true, some(LABEL), None), RestartAction::Exec);
        assert_eq!(
            restart_action(true, some(""), some("")),
            RestartAction::Exec
        );
    }

    #[test]
    fn mismatch_execs() {
        assert_eq!(
            restart_action(true, some(LABEL), some("com.apple.Terminal")),
            RestartAction::Exec
        );
        assert_eq!(
            restart_action(true, some(LABEL), some("0")),
            RestartAction::Exec
        );
    }

    #[test]
    fn xpc_zero_without_label_execs() {
        assert_eq!(restart_action(true, None, some("0")), RestartAction::Exec);
        assert_eq!(
            restart_action(true, some(""), some("0")),
            RestartAction::Exec
        );
    }

    #[test]
    fn non_macos_execs() {
        assert_eq!(
            restart_action(false, some(LABEL), some(LABEL)),
            RestartAction::Exec
        );
    }
}

/// Intercepts the private helper invocation before loading settings or a library.
/// Returns false for all normal invocations; applications must exit when true.
pub fn handle_update_helper() -> Result<bool> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_none_or(|arg| arg != "--audiolib-apply-update")
    {
        return Ok(false);
    }
    ensure!(arguments.len() == 2, "Invalid update helper invocation");
    apply_helper_plan(&PathBuf::from(&arguments[1]))?;
    Ok(true)
}

pub(crate) fn apply_helper_plan(plan_path: &Path) -> Result<()> {
    let plan_path = plan_path.canonicalize()?;
    let stage = plan_path
        .parent()
        .context("Missing update staging directory")?;
    validate_unlinked(stage)?;
    ensure!(
        stage
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".audiolib-update-"))
            && plan_path
                .file_name()
                .is_some_and(|name| name == "plan.json"),
        "Invalid update staging boundary"
    );
    ensure!(
        std::env::current_exe()?.canonicalize()?.parent() == Some(stage),
        "Update helper must execute from owned staging"
    );
    ensure!(
        std::fs::metadata(&plan_path)?.len() <= 8 * 1024 * 1024,
        "Update plan too large"
    );
    let plan: Plan = serde_json::from_slice(&std::fs::read(&plan_path)?)?;
    validate_install(&plan.install)?;
    ensure!(
        Some(plan.install.as_path()) == stage.parent(),
        "Staging does not belong to this installation"
    );
    let lock_path = plan.install.join(".audiolib-update.lock");
    validate_unlinked(&lock_path)?;
    let lock = OpenOptions::new().read(true).write(true).open(lock_path)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(error) => {
                record_result(
                    &plan,
                    stage,
                    &format!(
                        "Application did not exit before update handoff timeout; installation unchanged: {error}"
                    ),
                );
                bail!("Application did not release the update handoff lock: {error}");
            }
        }
    }
    let result = deploy(&plan, stage, None);
    match result {
        Ok(changed) => {
            if let Err(error) = restart_command(&plan).spawn() {
                if let Err(rollback_error) = rollback(&plan, stage, &changed) {
                    record_result(
                        &plan,
                        stage,
                        &format!(
                            "Restart failed ({error}); rollback incomplete. Recover from staging backup: {rollback_error:#}"
                        ),
                    );
                    return Err(rollback_error);
                }
                record_result(
                    &plan,
                    stage,
                    &format!("Restart failed; previous installation restored: {error}"),
                );
                restart_command(&plan)
                    .spawn()
                    .context("Restart previous application")?;
                return Ok(());
            }
            record_result(
                &plan,
                stage,
                "Update installed and application restarted.\n",
            );
        }
        Err(error) => {
            record_result(&plan, stage, &format!("Update failed: {error:#}"));
            if !error.is::<RollbackIncomplete>() {
                restart_command(&plan)
                    .spawn()
                    .context("Restart previous application")?;
            }
        }
    }
    Ok(())
}

fn record_result(plan: &Plan, stage: &Path, text: &str) {
    let _ = std::fs::write(stage.join("update-result.txt"), text);
    let path = plan.install.join(".audiolib-update-result.txt");
    if validate_unlinked(&path).is_ok() {
        let _ = std::fs::write(path, text);
    }
    eprintln!("AudioLib update: {text}");
}

#[derive(Debug)]
struct RollbackIncomplete {
    update: String,
    rollback: String,
}
impl std::fmt::Display for RollbackIncomplete {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Update failed ({}); rollback incomplete ({}); recovery backup retained",
            self.update, self.rollback
        )
    }
}
impl std::error::Error for RollbackIncomplete {}

fn restart_command(plan: &Plan) -> Command {
    let mut command = Command::new(plan.install.join(&plan.executable));
    command
        .args(&plan.arguments)
        .current_dir(&plan.working_directory);
    command
}

pub(crate) fn validate_install(directory: &Path) -> Result<()> {
    validate_unlinked(directory)?;
    let directory = directory.canonicalize()?;
    ensure!(
        directory.is_dir() && directory.parent().is_some(),
        "Invalid installation directory"
    );
    // Staging belongs inside the installation; /opt itself need not be writable.
    let probe = tempfile::Builder::new()
        .prefix(".audiolib-update-probe-")
        .tempfile_in(&directory)?;
    drop(probe);
    Ok(())
}

pub(crate) fn validate_unlinked(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "Linked update path is unsupported"
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        metadata.file_attributes() & 0x400 == 0,
                        "Reparse-point update path is unsupported"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn collect(
    root: &Path,
    directory: &Path,
    product: Product,
    files: &mut Vec<PlannedFile>,
) -> Result<()> {
    validate_unlinked(directory)?;
    for item in std::fs::read_dir(directory)? {
        let item = item?;
        let metadata = item.file_type()?;
        ensure!(!metadata.is_symlink(), "Linked update payload");
        if metadata.is_dir() {
            collect(root, &item.path(), product, files)?;
        } else {
            ensure!(metadata.is_file(), "Special update payload file");
            let name = item
                .path()
                .strip_prefix(root)?
                .to_str()
                .context("Invalid payload name")?
                .replace('\\', "/");
            ensure!(
                package::allowed(product, &name),
                "Non-public update payload file"
            );
            files.push(PlannedFile {
                name,
                hash: hash_file(&item.path())?,
            });
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Debug)]
struct Changed {
    name: String,
    had_original: bool,
}

fn deploy(plan: &Plan, stage: &Path, fail_after: Option<usize>) -> Result<Vec<Changed>> {
    validate_unlinked(stage)?;
    validate_install(&plan.install)?;
    ensure!(
        package::required(plan.product).contains(&plan.executable.as_str()),
        "Invalid restart executable"
    );
    let mut names = std::collections::HashSet::new();
    for file in &plan.files {
        ensure!(
            package::allowed(plan.product, &file.name)
                && names.insert(file.name.to_ascii_lowercase()),
            "Unsafe or duplicate planned update file"
        );
        let source = stage.join("payload").join(&file.name);
        validate_unlinked(&source)?;
        ensure!(
            source.is_file() && hash_file(&source)? == file.hash,
            "Staged update identity changed"
        );
        let target = plan.install.join(&file.name);
        validate_unlinked(&target)?;
        ensure!(
            !target.exists() || target.is_file(),
            "Update destination is not a regular file"
        );
    }
    for name in package::required(plan.product) {
        ensure!(
            names.contains(&name.to_ascii_lowercase()),
            "Incomplete update plan"
        );
    }
    let backup = stage.join("backup");
    std::fs::create_dir(&backup).context("Create unique update rollback directory")?;
    let mut changed = Vec::new();
    let result = (|| -> Result<()> {
        for file in &plan.files {
            let target = plan.install.join(&file.name);
            let old = backup.join(&file.name);
            validate_unlinked(&target)?;
            let had_original = target.exists();
            if had_original {
                std::fs::create_dir_all(old.parent().context("Missing rollback parent")?)?;
                std::fs::rename(&target, &old).context("Back up installed application file")?;
            }
            changed.push(Changed {
                name: file.name.clone(),
                had_original,
            });
            if fail_after == Some(changed.len()) {
                bail!("Injected failure after backing up an installed file");
            }
            std::fs::create_dir_all(target.parent().context("Missing installation parent")?)?;
            std::fs::rename(stage.join("payload").join(&file.name), &target)
                .context("Deploy verified application file")?;
            ensure!(
                hash_file(&target)? == file.hash,
                "Installed application hash mismatch"
            );
        }
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(rollback_error) = rollback(plan, stage, &changed) {
            return Err(RollbackIncomplete {
                update: format!("{error:#}"),
                rollback: format!("{rollback_error:#}"),
            }
            .into());
        }
        return Err(error);
    }
    Ok(changed)
}

fn rollback(plan: &Plan, stage: &Path, changed: &[Changed]) -> Result<()> {
    for file in changed.iter().rev() {
        let target = plan.install.join(&file.name);
        validate_unlinked(&target)?;
        if target.exists() {
            ensure!(target.is_file(), "Rollback target is not a file");
            std::fs::remove_file(&target)?;
        }
        if file.had_original {
            std::fs::rename(stage.join("backup").join(&file.name), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn exercise_deploy(install: &Path, stage: &Path, fail_after_backup: bool) -> Result<()> {
    let product = Product::Desktop;
    let mut files = Vec::new();
    collect(
        &stage.join("payload"),
        &stage.join("payload"),
        product,
        &mut files,
    )?;
    let plan = Plan {
        install: install.to_owned(),
        product,
        executable: "AudioLibExplorer.exe".into(),
        arguments: vec![],
        working_directory: install.to_owned(),
        files,
    };
    let result = deploy(&plan, stage, fail_after_backup.then_some(2));
    if fail_after_backup {
        ensure!(result.is_err(), "Expected injected deployment failure");
    } else {
        result?;
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod process_tests {
    use super::*;

    #[test]
    #[ignore = "Private child-process fixture, invoked by windows_helper_waits_for_parent_and_restarts"]
    fn helper_fixture() {
        apply_helper_plan(Path::new(
            &std::env::var_os("AUDIOLIB_UPDATE_TEST_PLAN").unwrap(),
        ))
        .unwrap();
    }

    #[test]
    #[ignore = "Private restart fixture, invoked by windows_helper_waits_for_parent_and_restarts"]
    fn restart_fixture() {
        assert_eq!(
            std::env::var("AUDIOLIB_UPDATE_TEST_SENTINEL").unwrap(),
            "inherited-value"
        );
        let marker = std::env::var_os("AUDIOLIB_UPDATE_TEST_MARKER").unwrap();
        std::fs::write(
            marker,
            std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn windows_helper_waits_for_parent_and_restarts() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        std::fs::create_dir(&install).unwrap();
        let stage = tempfile::Builder::new()
            .prefix(".audiolib-update-")
            .tempdir_in(&install)
            .unwrap();
        std::fs::create_dir(stage.path().join("payload")).unwrap();
        for name in package::required(Product::Desktop) {
            std::fs::write(install.join(name), b"old").unwrap();
            if *name == "AudioLibExplorer.exe" {
                std::fs::copy(
                    std::env::current_exe().unwrap(),
                    stage.path().join("payload").join(name),
                )
                .unwrap();
            } else {
                std::fs::write(stage.path().join("payload").join(name), b"new").unwrap();
            }
        }
        std::fs::write(install.join("settings.json"), b"preserved preferences").unwrap();
        let mut files = Vec::new();
        collect(
            &stage.path().join("payload"),
            &stage.path().join("payload"),
            Product::Desktop,
            &mut files,
        )
        .unwrap();
        let plan = Plan {
            install: install.canonicalize().unwrap(),
            product: Product::Desktop,
            executable: "AudioLibExplorer.exe".into(),
            arguments: [
                "--exact",
                "deploy::process_tests::restart_fixture",
                "--ignored",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
            working_directory: root.path().canonicalize().unwrap(),
            files,
        };
        let plan_path = stage.path().join("plan.json");
        std::fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(install.join(".audiolib-update.lock"))
            .unwrap();
        lock.lock().unwrap();
        let helper = stage.path().join("update-helper.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &helper).unwrap();
        let marker = root.path().join("restarted.txt");
        let mut child = Command::new(helper)
            .args([
                "--exact",
                "deploy::process_tests::helper_fixture",
                "--ignored",
            ])
            .env("AUDIOLIB_UPDATE_TEST_PLAN", &plan_path)
            .env("AUDIOLIB_UPDATE_TEST_MARKER", &marker)
            .env("AUDIOLIB_UPDATE_TEST_SENTINEL", "inherited-value")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            child.try_wait().unwrap().is_none(),
            "Helper exited before handoff lock released"
        );
        assert_eq!(
            std::fs::read(install.join("AudioLibCli.exe")).unwrap(),
            b"old"
        );
        assert!(!marker.exists());
        drop(lock);
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
            panic!("Fixture helper failed to finish after lock release");
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            PathBuf::from(std::fs::read_to_string(marker).unwrap())
                .canonicalize()
                .unwrap(),
            root.path().canonicalize().unwrap()
        );
        assert_eq!(
            std::fs::read(install.join("AudioLibCli.exe")).unwrap(),
            b"new"
        );
        assert_eq!(
            std::fs::read(install.join("settings.json")).unwrap(),
            b"preserved preferences"
        );
        assert_eq!(
            std::fs::read(stage.path().join("backup/AudioLibCli.exe")).unwrap(),
            b"old"
        );
    }
}
