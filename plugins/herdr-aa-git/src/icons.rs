//! File-type emoji, VS Code style — the same mapping as herdr-aa-filetree so the
//! two panels look like one suite. Icons avoid variation-selector (VS16) sequences
//! where practical — their rendered width is inconsistent across terminal
//! emulators and would misalign the list columns.

/// The emoji for a file name (source control lists only files, never directories).
pub fn icon_for(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if let Some(icon) = special_name(&lower) {
        return icon;
    }
    match lower.rsplit_once('.').map(|(_, ext)| ext) {
        Some(ext) => extension_icon(ext),
        None => "📄",
    }
}

/// Whole-filename matches take priority over the extension.
fn special_name(lower: &str) -> Option<&'static str> {
    let icon = match lower {
        "cargo.lock" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" => "🔒",
        "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "gemfile" => "📦",
        "makefile" | "justfile" | "cmakelists.txt" => "🔨",
        ".gitignore" | ".gitattributes" | ".gitmodules" => "🙈",
        _ if lower.starts_with("dockerfile") || lower.starts_with("docker-compose") => "🐳",
        _ if lower.starts_with("readme") => "📖",
        _ if lower.starts_with("license") || lower == "copying" => "📜",
        _ if lower == ".env" || lower.starts_with(".env.") => "🔑",
        _ => return None,
    };
    Some(icon)
}

fn extension_icon(ext: &str) -> &'static str {
    match ext {
        "rs" => "🦀",
        "py" | "pyi" => "🐍",
        "go" => "🐹",
        "rb" => "💎",
        "php" => "🐘",
        "java" | "jar" => "☕",
        "js" | "mjs" | "cjs" | "jsx" => "🟨",
        "ts" | "tsx" => "🔷",
        "json" | "jsonc" => "🧾",
        "md" | "markdown" => "📝",
        "html" | "htm" => "🌐",
        "css" | "scss" | "sass" | "less" => "🎨",
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => "🔧",
        "xml" => "📰",
        "sh" | "bash" | "zsh" | "fish" => "🐚",
        "ps1" | "psm1" | "psd1" | "bat" | "cmd" => "💻",
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" => "🔩",
        "cs" => "🟣",
        "kt" | "kts" => "🟪",
        "swift" => "🐦",
        "lua" => "🌙",
        "sql" | "db" | "sqlite" | "sqlite3" => "💾",
        "csv" | "tsv" => "📊",
        "txt" => "📄",
        "log" => "📋",
        "pdf" => "📕",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg" | "tiff" => "📷",
        "mp3" | "wav" | "flac" | "ogg" => "🎵",
        "mp4" | "mkv" | "avi" | "mov" | "webm" => "🎬",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" => "🧳",
        "lock" => "🔒",
        "exe" | "dll" | "so" | "dylib" | "a" | "o" | "bin" => "⚡",
        "wasm" => "🧩",
        "ttf" | "otf" | "woff" | "woff2" => "🔤",
        "ipynb" => "📓",
        _ => "📄",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_names_beat_extensions() {
        assert_eq!(icon_for("Cargo.toml"), "📦");
        assert_eq!(icon_for("Cargo.lock"), "🔒");
        assert_eq!(icon_for("README.md"), "📖");
        assert_eq!(icon_for("Dockerfile"), "🐳");
        assert_eq!(icon_for(".gitignore"), "🙈");
        assert_eq!(icon_for(".env.local"), "🔑");
    }

    #[test]
    fn extensions_are_case_insensitive() {
        assert_eq!(icon_for("MAIN.RS"), "🦀");
        assert_eq!(icon_for("photo.JPG"), "📷");
    }

    #[test]
    fn unknown_and_extensionless_fall_back_to_page() {
        assert_eq!(icon_for("data.xyzq"), "📄");
        assert_eq!(icon_for("CNAME"), "📄");
    }
}
