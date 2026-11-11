// app/audiolib-desktop/src/search_task_presentation.rs
//! Owned tab labels derived from executable conditions, never unfinished editor drafts.
use audiolib_core::{FileVisibility, SearchFilters, SortOrder, search_workspace::SearchTask};

pub(super) struct TaskPresentation {
    pub(super) name: String,
    pub(super) keywords: Vec<String>,
    pub(super) folder: String,
    pub(super) summary: String,
    pub(super) searched: bool,
}

fn default_title(title: &str) -> bool {
    matches!(title, "default" | "默认搜索")
        || ["搜索任务 ", "Search ", "Search task "]
            .iter()
            .any(|prefix| {
                title.strip_prefix(prefix).is_some_and(|number| {
                    !number.is_empty()
                        && number.chars().all(|character| character.is_ascii_digit())
                        && number.parse::<usize>().is_ok_and(|number| number > 0)
                })
            })
}

// This is presentation-only splitting. Preserve quoted phrases and the literal signs
// in names such as pass-by; the full original query remains in the hover summary.
fn query_keywords(query: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut closing_quote = None;
    for character in query.chars() {
        if let Some(close) = closing_quote {
            if character == close {
                closing_quote = None;
            } else {
                word.push(character);
            }
        } else if word.is_empty() && matches!(character, '"' | '“') {
            closing_quote = Some(if character == '“' { '”' } else { '"' });
        } else if character.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(character);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

// A backslash is part of a POSIX directory name, even on a Windows client.
fn folder_label(path: &str) -> String {
    let bytes = path.as_bytes();
    let windows = path.starts_with("\\\\")
        || path.starts_with("//")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'));
    let label = if windows {
        path.trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
    } else {
        path.trim_end_matches('/')
            .rsplit('/')
            .find(|part| !part.is_empty())
    };
    label.unwrap_or(path).to_owned()
}

pub(super) fn task_presentation(task: &SearchTask, chinese: bool) -> TaskPresentation {
    let text = |cn, en| if chinese { cn } else { en };
    let definition = &task.definition;
    let name = if !task.title_is_custom && default_title(&task.title) {
        text("默认搜索", "default").to_owned()
    } else {
        task.title.clone()
    };
    let folder = definition
        .folder
        .as_ref()
        .map_or_else(String::new, |folder| folder_label(&folder.path));
    let mut lines = vec![name.clone()];
    lines.push(format!(
        "{}: {}",
        text("搜索", "Search"),
        if definition.query.trim().is_empty() {
            text("全部声音", "All sounds")
        } else {
            &definition.query
        }
    ));
    lines.push(format!(
        "{}: {}",
        text("目录", "Folder"),
        definition
            .folder
            .as_ref()
            .map_or(text("全部目录", "All folders"), |folder| folder
                .path
                .as_str())
    ));
    lines.push(
        text(
            if definition.include_subfolders {
                "包含子目录"
            } else {
                "仅当前目录"
            },
            if definition.include_subfolders {
                "Include subfolders"
            } else {
                "Current folder only"
            },
        )
        .to_owned(),
    );
    let filters = &definition.filters;
    if let Some(binding) = &definition.album_binding {
        lines.push(format!(
            "{}: {}",
            text("专辑", "Album"),
            super::album_bindings::binding_label(binding, chinese)
        ));
    } else if let Some(name) = &filters.album {
        lines.push(format!("{}: {name}", text("专辑", "Album")));
    } else if let Some(id) = filters.album_id {
        lines.push(format!(
            "{}: {id} ({})",
            text("专辑", "Album"),
            text("身份不可用", "identity unavailable")
        ));
    }
    lines.push(format!(
        "{}: {}",
        text("文件状态", "File status"),
        match filters.visibility {
            FileVisibility::Present => text("可用文件", "Present files"),
            FileVisibility::Missing => text("缺失文件", "Missing files"),
            FileVisibility::All => text("全部文件", "All files"),
        }
    ));
    if let Some(value) = filters.sample_rate {
        lines.push(format!("{}: {value} Hz", text("采样率", "Sample rate")));
    }
    if let Some(value) = filters.bit_depth {
        lines.push(format!("{}: {value}", text("位深", "Bit depth")));
    }
    if let Some(value) = filters.channels {
        lines.push(format!("{}: {value}", text("声道", "Channels")));
    }
    if let Some(value) = filters.duration_min {
        lines.push(format!(
            "{}: {value} s",
            text("最短时长", "Minimum duration")
        ));
    }
    if let Some(value) = filters.duration_max {
        lines.push(format!(
            "{}: {value} s",
            text("最长时长", "Maximum duration")
        ));
    }
    if filters.favorites_only {
        lines.push(text("仅单个音频收藏", "Favorite audio only").into());
    }
    if filters.tagged_only {
        lines.push(text("仅有标签", "Tagged only").into());
    }
    if let Some(value) = &filters.tag_contains {
        lines.push(format!("{}: {value}", text("包含标签", "Include tag")));
    }
    for value in &filters.include_tags {
        lines.push(format!("{}: {value}", text("包含标签", "Include tag")));
    }
    for value in &filters.exclude_tags {
        lines.push(format!("{}: {value}", text("排除标签", "Exclude tag")));
    }
    if let Some(value) = &filters.color {
        lines.push(format!("{}: {value}", text("颜色", "Color")));
    }
    let sort = match definition.order_by {
        SortOrder::Relevance => text("相关性", "Relevance"),
        SortOrder::Filename => text("文件名", "Filename"),
        SortOrder::Duration => text("时长", "Duration"),
        SortOrder::SampleRate => text("采样率", "Sample rate"),
        SortOrder::Modified => text("修改时间", "Modified"),
        SortOrder::Channels => text("声道", "Channels"),
        SortOrder::Tags => text("标签", "Tags"),
    };
    lines.push(format!(
        "{}: {sort} ({})",
        text("排序", "Sort"),
        if definition.descending {
            text("降序", "descending")
        } else {
            text("升序", "ascending")
        }
    ));
    TaskPresentation {
        name,
        keywords: query_keywords(&definition.query),
        folder,
        summary: lines.join("\n"),
        searched: !definition.query.trim().is_empty()
            || definition.folder.is_some()
            || filters != &SearchFilters::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiolib_core::search_workspace::SearchFolderRef;

    #[test]
    fn legacy_auto_titles_follow_the_language_but_custom_titles_survive() {
        for title in [
            "搜索任务 1",
            "Search 6",
            "Search task 2",
            "default",
            "默认搜索",
        ] {
            let task = SearchTask {
                title: title.into(),
                ..Default::default()
            };
            assert_eq!(task_presentation(&task, true).name, "默认搜索");
            assert_eq!(task_presentation(&task, false).name, "default");
        }
        for title in [
            "Search 2 metal",
            "Search task",
            "Search 0",
            "My rain",
            "Task 1",
        ] {
            let task = SearchTask {
                title: title.into(),
                ..Default::default()
            };
            assert_eq!(task_presentation(&task, true).name, title);
        }
    }

    #[test]
    fn explicitly_named_tasks_keep_literal_names_even_when_they_look_automatic() {
        for title in [
            "Search 2",
            "Search task 3",
            "搜索任务 1",
            "default",
            "默认搜索",
            "My rain",
        ] {
            let task = SearchTask {
                title: title.into(),
                title_is_custom: true,
                ..Default::default()
            };
            for chinese in [false, true] {
                let display = task_presentation(&task, chinese);
                assert_eq!(display.name, title);
                assert_eq!(display.summary.lines().next(), Some(title));
            }
        }
    }

    #[test]
    fn keywords_preserve_phrases_and_folder_uses_either_path_separator() {
        let mut task = SearchTask::default();
        task.definition.query = "hard \"metal hit\" “汽车 经过” pass-by".into();
        for (path, label) in [
            ("X:\\Sound Library\\Vehicles\\", "Vehicles"),
            ("/mnt/sounds/Vehicles/", "Vehicles"),
            ("/mnt/sounds/Vehicles\\Take/", "Vehicles\\Take"),
        ] {
            task.definition.folder = Some(SearchFolderRef {
                id: 1,
                path: path.into(),
            });
            let display = task_presentation(&task, false);
            assert_eq!(
                display.keywords,
                ["hard", "metal hit", "汽车 经过", "pass-by"]
            );
            assert_eq!(display.folder, label);
            assert!(display.summary.contains(&task.definition.query));
            assert!(display.summary.contains(path));
            assert!(display.searched);
        }
    }

    #[test]
    fn summary_uses_executable_conditions_and_preserves_all_active_filters() {
        let mut task = SearchTask {
            title: "Rain".into(),
            ..Default::default()
        };
        task.view
            .raw_inputs
            .insert("query".into(), "unfinished draft".into());
        task.definition.filters = SearchFilters {
            visibility: FileVisibility::Missing,
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            channels: Some(2),
            duration_min: Some(1.5),
            duration_max: Some(5.0),
            favorites_only: true,
            tagged_only: true,
            tag_contains: Some("rain".into()),
            include_tags: vec!["outside".into()],
            exclude_tags: vec!["thunder".into()],
            color: Some("#123456".into()),
            album: None,
            album_id: None,
        };
        task.definition.include_subfolders = false;
        task.definition.order_by = SortOrder::Filename;
        task.definition.descending = true;
        let display = task_presentation(&task, false);
        assert!(display.searched);
        assert!(display.keywords.is_empty());
        assert!(!display.summary.contains("unfinished draft"));
        for value in [
            "Rain",
            "Missing files",
            "96000",
            "24",
            "2",
            "1.5",
            "5",
            "Favorite audio only",
            "Tagged only",
            "rain",
            "outside",
            "thunder",
            "#123456",
            "Current folder only",
            "Filename",
            "descending",
        ] {
            assert!(display.summary.contains(value), "Missing condition {value}");
        }
        assert!(!task_presentation(&SearchTask::default(), false).searched);
    }
}
