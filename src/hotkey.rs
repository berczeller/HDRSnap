//! Hotkey strings like "Ctrl+Shift+PrintScreen" <-> (modifiers, virtual key).

pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

const NAMED: &[(&str, u32)] = &[
    ("PrintScreen", 0x2C),
    ("Pause", 0x13),
    ("ScrollLock", 0x91),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Space", 0x20),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("Backspace", 0x08),
    ("Up", 0x26),
    ("Down", 0x28),
    ("Left", 0x25),
    ("Right", 0x27),
    ("Num0", 0x60),
    ("Num1", 0x61),
    ("Num2", 0x62),
    ("Num3", 0x63),
    ("Num4", 0x64),
    ("Num5", 0x65),
    ("Num6", 0x66),
    ("Num7", 0x67),
    ("Num8", 0x68),
    ("Num9", 0x69),
    ("`", 0xC0),
    ("-", 0xBD),
    ("=", 0xBB),
    ("[", 0xDB),
    ("]", 0xDD),
    (";", 0xBA),
    ("'", 0xDE),
    (",", 0xBC),
    (".", 0xBE),
    ("/", 0xBF),
    ("\\", 0xDC),
];

const ALIASES: &[(&str, &str)] = &[
    ("prtsc", "PrintScreen"),
    ("prtscr", "PrintScreen"),
    ("printscr", "PrintScreen"),
    ("print", "PrintScreen"),
    ("snapshot", "PrintScreen"),
    ("ins", "Insert"),
    ("del", "Delete"),
    ("pgup", "PageUp"),
    ("pgdn", "PageDown"),
    ("return", "Enter"),
];

pub fn parse(s: &str) -> Option<Hotkey> {
    let mut mods = 0;
    let mut vk = None;
    for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "shift" => mods |= MOD_SHIFT,
            "alt" => mods |= MOD_ALT,
            "win" | "windows" | "super" => mods |= MOD_WIN,
            _ => {
                if vk.is_some() {
                    return None; // two non-modifier keys
                }
                vk = Some(key_code(part)?);
            }
        }
    }
    Some(Hotkey { mods, vk: vk? })
}

fn key_code(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let canonical = ALIASES.iter().find(|(a, _)| *a == lower).map(|(_, c)| *c).unwrap_or(name);
    if let Some((_, vk)) = NAMED.iter().find(|(n, _)| n.eq_ignore_ascii_case(canonical)) {
        return Some(*vk);
    }
    let bytes = canonical.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
        return Some(bytes[0].to_ascii_uppercase() as u32);
    }
    if (lower.starts_with('f')) && lower.len() <= 3 {
        if let Ok(n) = lower[1..].parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1);
            }
        }
    }
    None
}

pub fn format(h: Hotkey) -> String {
    let mut parts: Vec<String> = Vec::new();
    if h.mods & MOD_CONTROL != 0 {
        parts.push("Ctrl".into());
    }
    if h.mods & MOD_SHIFT != 0 {
        parts.push("Shift".into());
    }
    if h.mods & MOD_ALT != 0 {
        parts.push("Alt".into());
    }
    if h.mods & MOD_WIN != 0 {
        parts.push("Win".into());
    }
    let key = if let Some((n, _)) = NAMED.iter().find(|(_, v)| *v == h.vk) {
        n.to_string()
    } else if (0x70..=0x87).contains(&h.vk) {
        format!("F{}", h.vk - 0x70 + 1)
    } else if (0x30..=0x5A).contains(&h.vk) {
        char::from_u32(h.vk).unwrap().to_string()
    } else {
        format!("0x{:02X}", h.vk)
    };
    parts.push(key);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(parse("PrintScreen"), Some(Hotkey { mods: 0, vk: 0x2C }));
        assert_eq!(parse("ctrl + shift + prtsc"), Some(Hotkey { mods: MOD_CONTROL | MOD_SHIFT, vk: 0x2C }));
        assert_eq!(parse("Alt+F12"), Some(Hotkey { mods: MOD_ALT, vk: 0x7B }));
        assert_eq!(parse("Win+Shift+s"), Some(Hotkey { mods: MOD_WIN | MOD_SHIFT, vk: b'S' as u32 }));
        assert_eq!(parse("Ctrl+A+B"), None);
        assert_eq!(parse("Ctrl"), None);
        assert_eq!(format(parse("shift+ctrl+f5").unwrap()), "Ctrl+Shift+F5");
    }
}
