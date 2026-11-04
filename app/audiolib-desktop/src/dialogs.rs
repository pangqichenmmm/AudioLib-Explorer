// app/audiolib-desktop/src/dialogs.rs
//! Native file selection runs on a worker using Windows dialogs or macOS osascript.
use std::path::PathBuf;

/// Select one filesystem folder with the Windows common dialog on a worker thread.
/// Cancellation returns `None` so the caller can retain the existing typed path.
#[cfg(windows)]
pub fn choose_folder(chinese: bool, owner: usize) -> Result<Option<PathBuf>, String> {
    use std::{ffi::c_void, os::windows::ffi::OsStringExt, ptr};

    #[repr(C)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }
    #[repr(C)]
    struct UnknownVTable {
        unused: [usize; 2],
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }
    #[repr(C)]
    struct DialogVTable {
        unknown: UnknownVTable,
        show: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
        before_options: [usize; 5],
        set_options: unsafe extern "system" fn(*mut c_void, u32) -> i32,
        get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
        before_title: [usize; 6],
        set_title: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
        before_result: [usize; 2],
        get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
    }
    #[repr(C)]
    struct ItemVTable {
        unknown: UnknownVTable,
        before_name: [usize; 2],
        get_display_name: unsafe extern "system" fn(*mut c_void, u32, *mut *mut u16) -> i32,
    }
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
        fn CoUninitialize();
        fn CoCreateInstance(
            class: *const Guid,
            outer: *mut c_void,
            context: u32,
            interface: *const Guid,
            object: *mut *mut c_void,
        ) -> i32;
        fn CoTaskMemFree(memory: *mut c_void);
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: The guard exists only after successful COM initialization on
            // this worker, and all interface guards have already been released.
            unsafe { CoUninitialize() };
        }
    }
    struct Object(*mut c_void);
    impl Drop for Object {
        fn drop(&mut self) {
            // SAFETY: Each successful COM output owns one reference; the first
            // three vtable entries use IUnknown's SDK layout for both interfaces.
            unsafe {
                let table = &**(self.0 as *const *const UnknownVTable);
                (table.release)(self.0);
            }
        }
    }
    struct TaskString(*mut u16);
    impl Drop for TaskString {
        fn drop(&mut self) {
            // SAFETY: IShellItem::GetDisplayName allocates this output using COM's
            // task allocator; the guard owns the returned allocation exactly once.
            unsafe { CoTaskMemFree(self.0.cast()) };
        }
    }
    let check = |result: i32| {
        if result < 0 {
            Err(format!(
                "{}: {:#x}",
                if chinese {
                    "Windows 文件夹选择窗口失败"
                } else {
                    "Windows folder dialog failed"
                },
                result as u32
            ))
        } else {
            Ok(())
        }
    };
    let class = Guid {
        data1: 0xdc1c5a9c,
        data2: 0xe88a,
        data3: 0x4dde,
        data4: [0xa5, 0xa1, 0x60, 0xf8, 0x2a, 0x20, 0xae, 0xf7],
    };
    let interface = Guid {
        data1: 0x42f85136,
        data2: 0xdb7e,
        data3: 0x439c,
        data4: [0x85, 0xf1, 0xe4, 0x07, 0x5d, 0x13, 0x5f, 0xc8],
    };
    let title = if chinese {
        "选择音效文件夹"
    } else {
        "Choose a sound folder"
    }
    .encode_utf16()
    .chain(Some(0))
    .collect::<Vec<_>>();
    // SAFETY: This fresh worker initializes an STA; no interface crosses threads.
    // GUIDs and vtable prefixes exactly match IFileDialog/IShellItem in the Windows
    // SDK. All output buffers, strings and owned references live through each call.
    unsafe {
        check(CoInitializeEx(ptr::null_mut(), 2))?;
        let _apartment = Apartment;
        let mut raw_dialog = ptr::null_mut();
        check(CoCreateInstance(
            &class,
            ptr::null_mut(),
            1,
            &interface,
            &mut raw_dialog,
        ))?;
        if raw_dialog.is_null() {
            return Err("Windows folder dialog returned no interface".into());
        }
        let dialog = Object(raw_dialog);
        let table = &**(dialog.0 as *const *const DialogVTable);
        let mut options = 0;
        check((table.get_options)(dialog.0, &mut options))?;
        // FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST.
        check((table.set_options)(dialog.0, options | 0x20 | 0x40 | 0x800))?;
        check((table.set_title)(dialog.0, title.as_ptr()))?;
        let result = (table.show)(dialog.0, owner as *mut c_void);
        if result as u32 == 0x8007_04c7 {
            return Ok(None);
        }
        check(result)?;
        let mut raw_item = ptr::null_mut();
        check((table.get_result)(dialog.0, &mut raw_item))?;
        if raw_item.is_null() {
            return Err("Windows folder dialog returned no folder".into());
        }
        let item = Object(raw_item);
        let item_table = &**(item.0 as *const *const ItemVTable);
        let mut raw_name = ptr::null_mut();
        // SIGDN_FILESYSPATH returns an allocated, null-terminated UTF-16 path.
        check((item_table.get_display_name)(
            item.0,
            0x8005_8000,
            &mut raw_name,
        ))?;
        if raw_name.is_null() {
            return Err("Windows folder dialog returned no filesystem path".into());
        }
        let name = TaskString(raw_name);
        let mut length = 0;
        while *name.0.add(length) != 0 {
            length += 1;
        }
        let path = std::ffi::OsString::from_wide(std::slice::from_raw_parts(name.0, length));
        Ok(Some(PathBuf::from(path)))
    }
}

/// Select one folder through the macOS system scripting helper on a worker.
#[cfg(target_os = "macos")]
pub fn choose_folder(chinese: bool, _owner: usize) -> Result<Option<PathBuf>, String> {
    macos_choose(
        "folder",
        if chinese {
            "选择音效文件夹"
        } else {
            "Choose a sound folder"
        },
        "",
        "",
    )
}

/// Native selection is unavailable on other desktop targets.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn choose_folder(_chinese: bool, _owner: usize) -> Result<Option<PathBuf>, String> {
    Err("Native folder selection requires Windows or macOS".into())
}

#[derive(Clone, Copy)]
pub enum FileKind {
    Database,
    NewLibrary,
    Wav,
    Image,
    JsonLines,
    Csv,
    Annotations,
}

#[cfg(windows)]
pub fn choose(
    save: bool,
    kind: FileKind,
    chinese: bool,
    owner: usize,
) -> Result<Option<PathBuf>, String> {
    use std::{ffi::c_void, mem::size_of};
    #[repr(C)]
    struct OpenFileName {
        size: u32,
        owner: *mut c_void,
        instance: *mut c_void,
        filter: *const u16,
        custom_filter: *mut u16,
        custom_size: u32,
        filter_index: u32,
        file: *mut u16,
        file_size: u32,
        title_file: *mut u16,
        title_file_size: u32,
        initial_dir: *const u16,
        title: *const u16,
        flags: u32,
        file_offset: u16,
        extension_offset: u16,
        default_extension: *const u16,
        custom_data: isize,
        hook: *const c_void,
        template: *const u16,
        reserved: *mut c_void,
        reserved_word: u32,
        flags_ex: u32,
    }
    #[link(name = "comdlg32")]
    unsafe extern "system" {
        fn GetOpenFileNameW(value: *mut OpenFileName) -> i32;
        fn GetSaveFileNameW(value: *mut OpenFileName) -> i32;
        fn CommDlgExtendedError() -> u32;
    }
    let (filter, ext) = match (kind, chinese) {
        (FileKind::Database | FileKind::NewLibrary, true) => {
            ("音效库 SQLite 数据库 (*.db)\0*.db\0所有文件\0*.*\0\0", "db")
        }
        (FileKind::Database | FileKind::NewLibrary, false) => (
            "Library SQLite database (*.db)\0*.db\0All files\0*.*\0\0",
            "db",
        ),
        (FileKind::Wav, _) => ("WAV / BWF\0*.wav\0\0", "wav"),
        (FileKind::Image, _) => ("PNG / JPEG\0*.png;*.jpg;*.jpeg\0\0", "png"),
        (FileKind::JsonLines, _) => ("JSON Lines (*.jsonl)\0*.jsonl\0\0", "jsonl"),
        (FileKind::Csv, _) => ("CSV (*.csv)\0*.csv\0\0", "csv"),
        (FileKind::Annotations, true) => ("音效库标注\0*.jsonl;*.csv\0所有文件\0*.*\0\0", "jsonl"),
        (FileKind::Annotations, false) => (
            "Library annotations\0*.jsonl;*.csv\0All files\0*.*\0\0",
            "jsonl",
        ),
    };
    let filter = filter.encode_utf16().collect::<Vec<_>>();
    let ext = ext.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let title = match (kind, save, chinese) {
        (FileKind::NewLibrary, _, true) => "新建音效库",
        (FileKind::NewLibrary, _, false) => "Create sound library",
        (FileKind::Database, true, true) => "备份音效库",
        (FileKind::Database, true, false) => "Back up sound library",
        (FileKind::Database, false, true) => "选择音效库数据库",
        (FileKind::Database, false, false) => "Choose sound library database",
        (FileKind::Wav, _, true) => "选择 WAV，修复文件引用",
        (FileKind::Wav, _, false) => "Choose WAV to repair file reference",
        (FileKind::Image, _, true) => "选择专辑封面",
        (FileKind::Image, _, false) => "Choose album artwork",
        (_, true, true) => "导出音效库标注",
        (_, true, false) => "Export library annotations",
        (_, false, true) => "导入音效库标注",
        (_, false, false) => "Import library annotations",
    }
    .encode_utf16()
    .chain(Some(0))
    .collect::<Vec<_>>();
    let mut file = vec![0u16; 32_768];
    if save {
        let date = chrono::Local::now().format("%Y-%m-%d");
        let name = match kind {
            FileKind::Database => format!("AudioLib-backup-{date}.db"),
            FileKind::NewLibrary => "AudioLib-library.db".into(),
            FileKind::Csv => format!("AudioLib-annotations-{date}.csv"),
            FileKind::JsonLines | FileKind::Annotations => {
                format!("AudioLib-annotations-{date}.jsonl")
            }
            FileKind::Wav | FileKind::Image => String::new(),
        };
        for (target, value) in file.iter_mut().zip(name.encode_utf16()) {
            *target = value;
        }
    }
    // SAFETY: The Win32 POD is zero-initializable. All pointed buffers remain alive for
    // the synchronous call, writable file capacity equals file_size, and no hook exists.
    let mut value: OpenFileName = unsafe { std::mem::zeroed() };
    value.size = size_of::<OpenFileName>() as u32;
    value.owner = owner as *mut c_void;
    value.filter = filter.as_ptr();
    value.filter_index = 1;
    value.file = file.as_mut_ptr();
    value.file_size = file.len() as u32;
    value.title = title.as_ptr();
    value.default_extension = ext.as_ptr();
    value.flags =
        0x0008_0000 | 0x0000_0008 | 0x0000_0800 | if save { 0x0000_0002 } else { 0x0000_1000 };
    // SAFETY: OPENFILENAMEW layout follows the Windows SDK, buffers and length fields
    // above are valid, and this worker owns the entire dialog lifetime.
    let accepted = unsafe {
        if save {
            GetSaveFileNameW(&mut value)
        } else {
            GetOpenFileNameW(&mut value)
        }
    };
    if accepted == 0 {
        // SAFETY: This thread made the immediately preceding common-dialog call.
        let error = unsafe { CommDlgExtendedError() };
        return if error == 0 {
            Ok(None)
        } else {
            Err(if chinese {
                format!("Windows 文件选择窗口失败：{error:#x}")
            } else {
                format!("Windows file dialog failed: {error:#x}")
            })
        };
    }
    let end = file
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(file.len());
    Ok(Some(PathBuf::from(String::from_utf16_lossy(&file[..end]))))
}

/// Select or name a file through the macOS system scripting helper on a worker.
#[cfg(target_os = "macos")]
pub fn choose(
    save: bool,
    kind: FileKind,
    chinese: bool,
    _owner: usize,
) -> Result<Option<PathBuf>, String> {
    let title = match (kind, save, chinese) {
        (FileKind::NewLibrary, _, true) => "新建音效库",
        (FileKind::NewLibrary, _, false) => "Create sound library",
        (FileKind::Database, true, true) => "备份音效库",
        (FileKind::Database, true, false) => "Back up sound library",
        (FileKind::Database, false, true) => "选择音效库数据库",
        (FileKind::Database, false, false) => "Choose sound library database",
        (FileKind::Wav, _, true) => "选择 WAV，修复文件引用",
        (FileKind::Wav, _, false) => "Choose WAV to repair file reference",
        (FileKind::Image, _, true) => "选择专辑封面",
        (FileKind::Image, _, false) => "Choose album artwork",
        (_, true, true) => "导出音效库标注",
        (_, true, false) => "Export library annotations",
        (_, false, true) => "导入音效库标注",
        (_, false, false) => "Import library annotations",
    };
    let types = match kind {
        FileKind::Database | FileKind::NewLibrary => "db\nsqlite\nsqlite3",
        FileKind::Wav => "wav",
        FileKind::Image => "png\njpg\njpeg",
        FileKind::JsonLines => "jsonl",
        FileKind::Csv => "csv",
        FileKind::Annotations => "jsonl\ncsv",
    };
    let date = chrono::Local::now().format("%Y-%m-%d");
    let name = if save {
        match kind {
            FileKind::Database => format!("AudioLib-backup-{date}.db"),
            FileKind::NewLibrary => "AudioLib-library.db".into(),
            FileKind::Csv => format!("AudioLib-annotations-{date}.csv"),
            FileKind::JsonLines | FileKind::Annotations => {
                format!("AudioLib-annotations-{date}.jsonl")
            }
            FileKind::Wav | FileKind::Image => String::new(),
        }
    } else {
        String::new()
    };
    macos_choose(if save { "save" } else { "open" }, title, &name, types)
}

#[cfg(target_os = "macos")]
fn macos_choose(
    mode: &str,
    prompt: &str,
    default_name: &str,
    types: &str,
) -> Result<Option<PathBuf>, String> {
    // Script text is constant. Paths and prompts never become AppleScript source
    // or shell commands, and no AppKit objects are created on this worker thread.
    const SCRIPT: &str = r#"on run argv
    try
        set selectionMode to item 1 of argv
        set dialogPrompt to item 2 of argv
        if selectionMode is "folder" then
            set chosenPath to choose folder with prompt dialogPrompt
        else if selectionMode is "save" then
            set chosenPath to choose file name with prompt dialogPrompt default name (item 3 of argv)
        else
            set allowedTypes to paragraphs of (item 4 of argv)
            set chosenPath to choose file with prompt dialogPrompt of type allowedTypes
        end if
        return POSIX path of chosenPath
    on error errorMessage number errorNumber
        if errorNumber is -128 then return ""
        error errorMessage number errorNumber
    end try
end run"#;
    let output = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(SCRIPT)
        .args([mode, prompt, default_name, types])
        .output()
        .map_err(|error| format!("macOS file dialog could not start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "macOS file dialog failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        ));
    }
    macos_dialog_path(&output.stdout)
}

#[cfg(any(target_os = "macos", test))]
fn macos_dialog_path(output: &[u8]) -> Result<Option<PathBuf>, String> {
    let path = std::str::from_utf8(output)
        .map_err(|_| "macOS file dialog returned an invalid UTF-8 path".to_owned())?;
    // osascript adds exactly one line terminator. A filename may itself end in
    // spaces or a newline, so trimming whitespace would change the selected file.
    let path = path.strip_suffix('\n').unwrap_or(path);
    if path.is_empty() {
        return Ok(None);
    }
    if !path.starts_with('/') || path.contains('\0') {
        return Err("macOS file dialog returned an invalid absolute path".into());
    }
    Ok(Some(PathBuf::from(path)))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn choose(
    _save: bool,
    _kind: FileKind,
    _chinese: bool,
    _owner: usize,
) -> Result<Option<PathBuf>, String> {
    Err("Native file selection requires Windows or macOS".into())
}

/// Capture the main window's HWND on the UI thread before starting a dialog worker.
/// A zero handle is used by headless tests and platforms without a Win32 window.
pub fn owner_handle(window: &slint::Window) -> usize {
    #[cfg(windows)]
    {
        use slint::winit_030::{
            WinitWindowAccessor,
            winit::raw_window_handle::{HasWindowHandle, RawWindowHandle},
        };
        window
            .with_winit_window(|native| {
                native
                    .window_handle()
                    .ok()
                    .and_then(|handle| match handle.as_raw() {
                        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as usize),
                        _ => None,
                    })
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::macos_dialog_path;
    use std::path::PathBuf;

    #[test]
    fn macos_dialog_preserves_unicode_and_filename_whitespace() {
        for selected in ["/Users/test/音效 library ", "/Users/test/newline\n"] {
            let stdout = format!("{selected}\n");
            assert_eq!(
                macos_dialog_path(stdout.as_bytes()).unwrap(),
                Some(PathBuf::from(selected))
            );
        }
    }

    #[test]
    fn macos_dialog_cancellation_does_not_produce_a_path() {
        assert_eq!(macos_dialog_path(b"\n").unwrap(), None);
        assert_eq!(macos_dialog_path(b"").unwrap(), None);
    }

    #[test]
    fn macos_dialog_rejects_invalid_or_non_absolute_output() {
        for invalid in [
            b"relative.wav\n".as_slice(),
            b"/file\0.wav\n",
            b"/file\xff\n",
        ] {
            assert!(macos_dialog_path(invalid).is_err());
        }
    }
}
