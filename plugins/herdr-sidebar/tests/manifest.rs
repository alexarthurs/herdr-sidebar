//! The manifest is read by herdr, not by our code, so nothing else would
//! notice it breaking. A Windows hook once used `".\target\release\…"`: `\t`
//! and `\r` are valid TOML escapes and `\h` is not, so the whole manifest
//! failed to parse and the plugin stopped loading on every platform.

fn manifest() -> toml::Table {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/herdr-plugin.toml"))
        .expect("read herdr-plugin.toml");
    text.parse().expect("herdr-plugin.toml is valid TOML")
}

#[test]
fn every_hook_and_action_command_survives_toml_parsing() {
    let manifest = manifest();
    let mut commands = 0;
    for section in ["events", "actions"] {
        for entry in manifest
            .get(section)
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let program = entry
                .get("command")
                .and_then(|c| c.as_array())
                .and_then(|argv| argv.first())
                .and_then(|p| p.as_str())
                .unwrap_or_else(|| panic!("{section} entry without a command: {entry:?}"));
            assert!(
                !program.chars().any(char::is_control),
                "{section} program has a control character (a TOML escape?): {program:?}"
            );
            if program.contains("release") {
                assert!(
                    program.starts_with("./target/release/")
                        || program.starts_with(".\\target\\release\\"),
                    "{section} program is not a plugin-relative binary: {program:?}"
                );
            }
            commands += 1;
        }
    }
    assert!(commands > 0, "no hooks or actions found");
}
