/// Terminal color capability tiers, from most to least capable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorCapability {
    TrueColor,
    Color256,
    Color16,
    Monochrome,
}

impl std::fmt::Display for ColorCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ColorCapability::TrueColor => write!(f, "truecolor"),
            ColorCapability::Color256 => write!(f, "256color"),
            ColorCapability::Color16 => write!(f, "16color"),
            ColorCapability::Monochrome => write!(f, "mono"),
        }
    }
}

/// Detect terminal color capability from environment variables.
///
/// Priority order:
/// 1. `$NO_COLOR` set (any value) → Monochrome
/// 2. `$COLORTERM` contains "truecolor" or "24bit" → TrueColor
/// 3. `$TERM` contains "256color" → Color256
/// 4. Otherwise → Color16
pub fn detect_color_capability() -> ColorCapability {
    if std::env::var("NO_COLOR").is_ok() {
        return ColorCapability::Monochrome;
    }

    if let Ok(colorterm) = std::env::var("COLORTERM") {
        let ct = colorterm.to_lowercase();
        if ct.contains("truecolor") || ct.contains("24bit") {
            return ColorCapability::TrueColor;
        }
    }

    if let Ok(term) = std::env::var("TERM") {
        if term.contains("256color") {
            return ColorCapability::Color256;
        }
    }

    ColorCapability::Color16
}
