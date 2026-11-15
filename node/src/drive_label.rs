//! Human-readable mapped-drive names; transport identities stay separate.

fn parts(label: &str) -> (&str, Option<&str>) {
    let label = label.trim();
    if label.ends_with(')') {
        let mut depth = 0;
        for (index, character) in label.char_indices().rev() {
            match character {
                ')' => depth += 1,
                '(' => {
                    depth -= 1;
                    if depth == 0 {
                        if index > 0 && label[..index].ends_with(' ') {
                            if let Some(source) = label[index + 1..label.len() - 1]
                                .strip_prefix("on ")
                                .filter(|source| !source.trim().is_empty())
                            {
                                return (label[..index].trim_end(), Some(source));
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    (label, None)
}

pub(crate) fn mapped_drive_label(drive: &str, source: &str) -> String {
    let (drive, _) = parts(drive);
    let drive = if drive.is_empty() { "Drive" } else { drive };
    let source = source.trim();
    let source = if source.is_empty() { "Device" } else { source };
    format!("{drive} (on {source})")
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
fn shortened(text: &str, budget: usize, width: fn(char) -> usize) -> String {
    if text.chars().map(width).sum::<usize>() <= budget {
        return text.into();
    }
    let mut remaining = budget.saturating_sub(width('…'));
    let mut result = String::new();
    for character in text.chars() {
        let size = width(character);
        if size > remaining {
            break;
        }
        result.push(character);
        remaining -= size;
    }
    result.push('…');
    result
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
fn native_label(label: &str, budget: usize, width: fn(char) -> usize) -> String {
    let (drive, source) = parts(label);
    let Some(source) = source else {
        return shortened(label, budget, width);
    };
    let available = budget - " (on )".len();
    let source_size = source.chars().map(width).sum::<usize>();
    let drive_size = drive.chars().map(width).sum::<usize>();
    // Keep both identities visible. Give unused space back to the longer one.
    let source_budget = source_size.min(available.saturating_sub(drive_size.min(available / 2)));
    let drive_budget = available - source_budget;
    format!(
        "{} (on {})",
        shortened(drive, drive_budget, width),
        shortened(source, source_budget, width)
    )
}

#[cfg(any(windows, test))]
pub(crate) fn windows_drive_label(label: &str) -> String {
    // Explorer's custom label is separate from the filesystem volume label.
    // Bound the display string without losing the full name in AMS metadata.
    native_label(label, 255, char::len_utf16)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) fn unix_mount_name(label: &str) -> String {
    let cleaned = label
        .trim()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.' | '(' | ')')
            {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let cleaned = cleaned.trim_matches([' ', '.']).trim();
    let cleaned = if cleaned.is_empty() {
        "Remote Drive"
    } else {
        cleaned
    };
    // A mount-point component has a 255-byte budget; reserve " 1000" for collisions.
    native_label(cleaned, 250, char::len_utf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_is_added_once_and_can_be_renamed() {
        assert_eq!(
            mapped_drive_label(" Photos (raw) ", " Laptop "),
            "Photos (raw) (on Laptop)"
        );
        assert_eq!(
            mapped_drive_label("Photos (on Laptop (work))", "Laptop (work)"),
            "Photos (on Laptop (work))"
        );
        assert_eq!(
            mapped_drive_label("Photos (on Old name)", "Laptop"),
            "Photos (on Laptop)"
        );
    }

    #[test]
    fn native_limits_preserve_both_names_and_unicode_boundaries() {
        let label = mapped_drive_label(&"📁資料".repeat(100), &"Laptop資料".repeat(100));
        let windows = windows_drive_label(&label);
        assert!(windows.encode_utf16().count() <= 255);
        assert!(windows.starts_with("📁資料"));
        assert!(windows.contains(" (on Laptop資料"));
        assert!(windows.ends_with(')'));
        let unix = unix_mount_name(&label);
        assert!(unix.len() <= 250);
        assert!(unix.contains(" (on Laptop資料"));
        assert!(unix.ends_with(')'));
        assert!(format!("{unix} 1000").len() <= 255);
    }

    #[test]
    fn short_names_stay_readable_and_mount_paths_are_safe() {
        let label = "chrispaul (on Laptop)";
        assert_eq!(windows_drive_label(label), label);
        assert_eq!(unix_mount_name(label), label);
        assert_eq!(
            unix_mount_name("Photos/2026 (on Laptop)"),
            "Photos-2026 (on Laptop)"
        );
    }
}
