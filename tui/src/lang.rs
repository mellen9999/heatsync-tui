//! which language the ui speaks: the `lang=` line in the config (`auto` = the
//! os locale), else LC_ALL / LC_MESSAGES / LANG, else the windows user locale.
//! the lookup itself lives in heatsync-core's i18n module.

use std::sync::Mutex;

use heatsync_core::i18n;

use crate::config;

/// what the config says (`auto` or a code) — kept so a save writes it back.
static SETTING: Mutex<String> = Mutex::new(String::new());

pub fn setting() -> String {
    match SETTING.lock() {
        Ok(s) if !s.is_empty() => s.clone(),
        _ => "auto".into(),
    }
}

/// os locale string, "" when unknown.
pub fn os_locale() -> String {
    let env = i18n::detect(|k| std::env::var(k).ok());
    if !env.is_empty() {
        return env;
    }
    windows_locale()
}

#[cfg(windows)]
fn windows_locale() -> String {
    extern "system" {
        fn GetUserDefaultLocaleName(buf: *mut u16, len: i32) -> i32;
    }
    let mut buf = [0u16; 85];
    // SAFETY: buf is 85 wide chars (LOCALE_NAME_MAX_LENGTH); the call writes at
    // most that many and returns the count including the nul, 0 on failure.
    let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 1 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..(n - 1) as usize])
}

#[cfg(not(windows))]
fn windows_locale() -> String {
    String::new()
}

/// apply a setting (`auto` or a code) to the running process.
pub fn apply(setting_value: &str) {
    if let Ok(mut s) = SETTING.lock() {
        *s = setting_value.to_string();
    }
    i18n::set(setting_value, &os_locale());
}

/// `lang <code|auto>`: no argument shows the current + available.
pub fn run(args: &[String]) -> std::io::Result<()> {
    use heatsync_core::i18n::trf;
    let Some(want) = args.first().map(|w| w.trim()) else {
        println!(
            "{}",
            trf(
                "lang.current",
                &[("lang", i18n::current()), ("setting", &setting())]
            )
        );
        println!("auto {}", i18n::LOCALES.join(" "));
        return Ok(());
    };
    let canon = if want.eq_ignore_ascii_case("auto") {
        Some("auto")
    } else {
        i18n::LOCALES
            .iter()
            .find(|l| l.eq_ignore_ascii_case(want))
            .copied()
    };
    let Some(code) = canon else {
        eprintln!("{}", trf("lang.unknown", &[("lang", want)]));
        std::process::exit(2);
    };
    let mut cfg = config::load();
    cfg.lang = code.to_string();
    config::save(&cfg);
    apply(code);
    println!("{}", trf("lang.set", &[("lang", i18n::current())]));
    Ok(())
}
