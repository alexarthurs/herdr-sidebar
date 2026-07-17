//! File-type icons, VS Code Explorer style, in two selectable themes:
//!
//! - `Emoji` (default): colored emoji, renders in any terminal font. Avoids
//!   variation-selector (VS16) sequences — their rendered width is inconsistent
//!   across terminal emulators and would misalign the tree columns.
//! - `Material`: Nerd Font glyphs colored like the VS Code "Atom Material Icons"
//!   theme. Requires herdr's terminal font to be Nerd-Font-patched; the `i` key
//!   toggles themes live, so a font without the glyphs is one keypress away from
//!   emoji again.
//!
//! Classification happens once (`Kind`), so both themes always agree on what a
//! file is and only differ in how they draw it.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconTheme {
    Emoji,
    Material,
}

impl IconTheme {
    /// Initial theme from `HERDR_AA_FILETREE_ICONS` (`material`/`nerd` → Material).
    pub fn from_env(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_lowercase()).as_deref() {
            Some("material") | Some("nerd") => Self::Material,
            _ => Self::Emoji,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Emoji => Self::Material,
            Self::Material => Self::Emoji,
        }
    }
}

/// A renderable icon: the glyph plus an optional foreground color. Emoji carry
/// their own colors (`None`); material glyphs are tinted like Atom Material.
pub struct Icon {
    pub glyph: &'static str,
    pub rgb: Option<(u8, u8, u8)>,
}

pub fn icon(theme: IconTheme, name: &str, is_dir: bool, expanded: bool) -> Icon {
    let kind = kind_of(name, is_dir, expanded);
    match theme {
        IconTheme::Emoji => Icon { glyph: emoji(kind), rgb: None },
        IconTheme::Material => {
            let (glyph, rgb) = material(kind);
            Icon { glyph, rgb: Some(rgb) }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Dir,
    DirOpen,
    Rust,
    Python,
    Js,
    Ts,
    React,
    Json,
    Markdown,
    Html,
    Css,
    Config,
    Xml,
    Shell,
    PowerShell,
    CFamily,
    CSharp,
    Go,
    Ruby,
    Php,
    Java,
    Kotlin,
    Swift,
    Lua,
    Sql,
    Data,
    Text,
    Log,
    Pdf,
    Image,
    Audio,
    Video,
    Archive,
    Lock,
    Binary,
    Font,
    Notebook,
    Git,
    Docker,
    Package,
    Build,
    Readme,
    License,
    EnvKey,
    File,
}

fn kind_of(name: &str, is_dir: bool, expanded: bool) -> Kind {
    if is_dir {
        return if expanded { Kind::DirOpen } else { Kind::Dir };
    }
    let lower = name.to_lowercase();
    if let Some(kind) = special_name(&lower) {
        return kind;
    }
    match lower.rsplit_once('.').map(|(_, ext)| ext) {
        Some(ext) => extension_kind(ext),
        None => Kind::File,
    }
}

/// Whole-filename matches take priority over the extension.
fn special_name(lower: &str) -> Option<Kind> {
    let kind = match lower {
        "cargo.lock" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" => Kind::Lock,
        "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "gemfile" => Kind::Package,
        "makefile" | "justfile" | "cmakelists.txt" => Kind::Build,
        ".gitignore" | ".gitattributes" | ".gitmodules" => Kind::Git,
        _ if lower.starts_with("dockerfile") || lower.starts_with("docker-compose") => Kind::Docker,
        _ if lower.starts_with("readme") => Kind::Readme,
        _ if lower.starts_with("license") || lower == "copying" => Kind::License,
        _ if lower == ".env" || lower.starts_with(".env.") => Kind::EnvKey,
        _ => return None,
    };
    Some(kind)
}

fn extension_kind(ext: &str) -> Kind {
    match ext {
        "rs" => Kind::Rust,
        "py" | "pyi" => Kind::Python,
        "js" | "mjs" | "cjs" => Kind::Js,
        "ts" => Kind::Ts,
        "jsx" | "tsx" => Kind::React,
        "json" | "jsonc" => Kind::Json,
        "md" | "markdown" => Kind::Markdown,
        "html" | "htm" => Kind::Html,
        "css" | "scss" | "sass" | "less" => Kind::Css,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => Kind::Config,
        "xml" => Kind::Xml,
        "sh" | "bash" | "zsh" | "fish" => Kind::Shell,
        "ps1" | "psm1" | "psd1" | "bat" | "cmd" => Kind::PowerShell,
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" => Kind::CFamily,
        "cs" => Kind::CSharp,
        "go" => Kind::Go,
        "rb" => Kind::Ruby,
        "php" => Kind::Php,
        "java" | "jar" => Kind::Java,
        "kt" | "kts" => Kind::Kotlin,
        "swift" => Kind::Swift,
        "lua" => Kind::Lua,
        "sql" | "db" | "sqlite" | "sqlite3" => Kind::Sql,
        "csv" | "tsv" => Kind::Data,
        "txt" => Kind::Text,
        "log" => Kind::Log,
        "pdf" => Kind::Pdf,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg" | "tiff" => Kind::Image,
        "mp3" | "wav" | "flac" | "ogg" => Kind::Audio,
        "mp4" | "mkv" | "avi" | "mov" | "webm" => Kind::Video,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" => Kind::Archive,
        "lock" => Kind::Lock,
        "exe" | "dll" | "so" | "dylib" | "a" | "o" | "bin" | "wasm" => Kind::Binary,
        "ttf" | "otf" | "woff" | "woff2" => Kind::Font,
        "ipynb" => Kind::Notebook,
        _ => Kind::File,
    }
}

fn emoji(kind: Kind) -> &'static str {
    match kind {
        Kind::Dir => "📁",
        Kind::DirOpen => "📂",
        Kind::Rust => "🦀",
        Kind::Python => "🐍",
        Kind::Js => "🟨",
        Kind::Ts => "🔷",
        Kind::React => "🟦",
        Kind::Json => "🧾",
        Kind::Markdown => "📝",
        Kind::Html => "🌐",
        Kind::Css => "🎨",
        Kind::Config => "🔧",
        Kind::Xml => "📰",
        Kind::Shell => "🐚",
        Kind::PowerShell => "💻",
        Kind::CFamily => "🔩",
        Kind::CSharp => "🟣",
        Kind::Go => "🐹",
        Kind::Ruby => "💎",
        Kind::Php => "🐘",
        Kind::Java => "☕",
        Kind::Kotlin => "🟪",
        Kind::Swift => "🐦",
        Kind::Lua => "🌙",
        Kind::Sql => "💾",
        Kind::Data => "📊",
        Kind::Text => "📄",
        Kind::Log => "📋",
        Kind::Pdf => "📕",
        Kind::Image => "📷",
        Kind::Audio => "🎵",
        Kind::Video => "🎬",
        Kind::Archive => "🧳",
        Kind::Lock => "🔒",
        Kind::Binary => "⚡",
        Kind::Font => "🔤",
        Kind::Notebook => "📓",
        Kind::Git => "🙈",
        Kind::Docker => "🐳",
        Kind::Package => "📦",
        Kind::Build => "🔨",
        Kind::Readme => "📖",
        Kind::License => "📜",
        Kind::EnvKey => "🔑",
        Kind::File => "📄",
    }
}

/// Nerd Font glyph + Atom-Material-style color per kind. Codepoints are Nerd
/// Fonts v3 (devicons/codicons/Font Awesome/Material Design ranges).
fn material(kind: Kind) -> (&'static str, (u8, u8, u8)) {
    match kind {
        Kind::Dir => ("\u{f07b}", (0x90, 0xa4, 0xae)),      //  blue-grey folder
        Kind::DirOpen => ("\u{f07c}", (0x90, 0xa4, 0xae)),  //  open folder
        Kind::Rust => ("\u{e7a8}", (0xde, 0xa5, 0x84)),     //  rust orange
        Kind::Python => ("\u{e73c}", (0x35, 0x72, 0xa5)),   //  python blue
        Kind::Js => ("\u{e74e}", (0xf1, 0xe0, 0x5a)),       //  js yellow
        Kind::Ts => ("\u{e628}", (0x31, 0x78, 0xc6)),       //  ts blue
        Kind::React => ("\u{e7ba}", (0x61, 0xda, 0xfb)),    //  react cyan
        Kind::Json => ("\u{e60b}", (0xcb, 0xcb, 0x41)),     //  json yellow
        Kind::Markdown => ("\u{f48a}", (0x51, 0x9a, 0xba)), //  markdown blue
        Kind::Html => ("\u{e736}", (0xe3, 0x4c, 0x26)),     //  html orange
        Kind::Css => ("\u{e749}", (0x42, 0xa5, 0xf5)),      //  css blue
        Kind::Config => ("\u{e615}", (0x6d, 0x80, 0x86)),   //  gear grey
        Kind::Xml => ("\u{f121}", (0xe3, 0x79, 0x33)),      //  code orange
        Kind::Shell => ("\u{f489}", (0x4e, 0xaa, 0x25)),    //  shell green
        Kind::PowerShell => ("\u{f0a0a}", (0x53, 0x91, 0xfe)), // 󰨊 powershell blue
        Kind::CFamily => ("\u{e61d}", (0xf3, 0x4b, 0x7d)),  //  c/cpp pink
        Kind::CSharp => ("\u{f031b}", (0x17, 0x86, 0x00)),  // 󰌛 c# green
        Kind::Go => ("\u{e627}", (0x00, 0xad, 0xd8)),       //  go cyan
        Kind::Ruby => ("\u{e791}", (0x70, 0x15, 0x16)),     //  ruby red
        Kind::Php => ("\u{e73d}", (0x4f, 0x5d, 0x95)),      //  php indigo
        Kind::Java => ("\u{e738}", (0xb0, 0x72, 0x19)),     //  java brown
        Kind::Kotlin => ("\u{e634}", (0xa9, 0x7b, 0xff)),   //  kotlin purple
        Kind::Swift => ("\u{e755}", (0xf0, 0x51, 0x38)),    //  swift orange
        Kind::Lua => ("\u{e620}", (0x51, 0xa0, 0xcf)),      //  lua blue
        Kind::Sql => ("\u{e706}", (0xf2, 0x91, 0x11)),      //  db orange
        Kind::Data => ("\u{f1c3}", (0x33, 0xa8, 0x52)),     //  sheet green
        Kind::Text => ("\u{f15c}", (0x9e, 0x9e, 0x9e)),     //  text grey
        Kind::Log => ("\u{f15c}", (0x75, 0x75, 0x75)),      //  log dark grey
        Kind::Pdf => ("\u{f1c1}", (0xe5, 0x39, 0x35)),      //  pdf red
        Kind::Image => ("\u{f1c5}", (0x26, 0xa6, 0x9a)),    //  image teal
        Kind::Audio => ("\u{f1c7}", (0xec, 0x40, 0x7a)),    //  audio pink
        Kind::Video => ("\u{f1c8}", (0xff, 0x70, 0x43)),    //  video orange
        Kind::Archive => ("\u{f1c6}", (0xaf, 0xb4, 0x2b)),  //  archive olive
        Kind::Lock => ("\u{f023}", (0xff, 0xd5, 0x4f)),     //  lock amber
        Kind::Binary => ("\u{f471}", (0xef, 0x53, 0x50)),   //  binary red
        Kind::Font => ("\u{f031}", (0xb0, 0xbe, 0xc5)),     //  font grey
        Kind::Notebook => ("\u{f02d}", (0xf5, 0x7c, 0x00)), //  notebook orange
        Kind::Git => ("\u{e702}", (0xf1, 0x4e, 0x32)),      //  git orange-red
        Kind::Docker => ("\u{f308}", (0x0d, 0xb7, 0xed)),   //  docker blue
        Kind::Package => ("\u{f487}", (0x8d, 0x6e, 0x63)),  //  package brown
        Kind::Build => ("\u{f0ad}", (0x6d, 0x80, 0x86)),    //  wrench grey
        Kind::Readme => ("\u{f02d}", (0x42, 0xa5, 0xf5)),   //  book blue
        Kind::License => ("\u{f24e}", (0xff, 0xd5, 0x4f)),  //  scale amber
        Kind::EnvKey => ("\u{f084}", (0xff, 0xd5, 0x4f)),   //  key amber
        Kind::File => ("\u{f15b}", (0x90, 0xa4, 0xae)),     //  plain file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emoji_for(name: &str, is_dir: bool, expanded: bool) -> &'static str {
        icon(IconTheme::Emoji, name, is_dir, expanded).glyph
    }

    #[test]
    fn directories_reflect_expansion() {
        assert_eq!(emoji_for("src", true, false), "📁");
        assert_eq!(emoji_for("src", true, true), "📂");
    }

    #[test]
    fn special_names_beat_extensions() {
        assert_eq!(emoji_for("Cargo.toml", false, false), "📦");
        assert_eq!(emoji_for("Cargo.lock", false, false), "🔒");
        assert_eq!(emoji_for("README.md", false, false), "📖");
        assert_eq!(emoji_for("Dockerfile", false, false), "🐳");
        assert_eq!(emoji_for(".gitignore", false, false), "🙈");
        assert_eq!(emoji_for(".env.local", false, false), "🔑");
    }

    #[test]
    fn extensions_are_case_insensitive() {
        assert_eq!(emoji_for("MAIN.RS", false, false), "🦀");
        assert_eq!(emoji_for("photo.JPG", false, false), "📷");
    }

    #[test]
    fn unknown_and_extensionless_fall_back_to_file() {
        assert_eq!(emoji_for("data.xyzq", false, false), "📄");
        assert_eq!(emoji_for("CNAME", false, false), "📄");
    }

    #[test]
    fn material_theme_tints_glyphs() {
        let rust = icon(IconTheme::Material, "main.rs", false, false);
        assert_eq!(rust.glyph, "\u{e7a8}");
        assert_eq!(rust.rgb, Some((0xde, 0xa5, 0x84)));
        assert!(icon(IconTheme::Emoji, "main.rs", false, false).rgb.is_none());
    }

    #[test]
    fn theme_selection_from_env_and_toggle() {
        assert_eq!(IconTheme::from_env(None), IconTheme::Emoji);
        assert_eq!(IconTheme::from_env(Some("material")), IconTheme::Material);
        assert_eq!(IconTheme::from_env(Some(" NERD ")), IconTheme::Material);
        assert_eq!(IconTheme::from_env(Some("emoji")), IconTheme::Emoji);
        assert_eq!(IconTheme::Emoji.toggled(), IconTheme::Material);
        assert_eq!(IconTheme::Material.toggled(), IconTheme::Emoji);
    }
}
