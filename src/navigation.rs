//! Navigation symbols are optional decoration. Labels remain usable with fonts
//! that have no icon glyphs, and when the terminal's font cannot be inspected.

#[derive(Debug, Clone, Copy)]
pub enum Icon {
    Home,
    Playing,
    Menu,
    Search,
}

/// Bits correspond to GLYPHS, including alternatives for common console fonts.
#[derive(Debug, Clone, Copy, Default)]
pub struct Symbols(u32);

impl Symbols {
    pub const NONE: Self = Self(0);
    #[cfg(test)]
    pub const ALL: Self = Self(0x3f);

    #[cfg(any(windows, test))]
    pub const fn from_mask(mask: u32) -> Self {
        if mask & 0x3f == 0 {
            Self::NONE
        } else {
            Self(mask & 0x3f)
        }
    }

    pub fn symbol(self, icon: Icon) -> &'static str {
        let choices: &[(u32, &str)] = match icon {
            Icon::Home => &[(0, "⌂")],
            Icon::Playing => &[(1, "▶"), (2, "►")],
            Icon::Menu => &[(3, "☰"), (4, "≡")],
            Icon::Search => &[(5, "⌕")],
        };
        choices
            .iter()
            .find_map(|(bit, symbol)| (self.0 & (1 << bit) != 0).then_some(*symbol))
            .unwrap_or("")
    }

    pub fn label(self, icon: Icon, label: &str) -> String {
        let symbol = self.symbol(icon);
        if symbol.is_empty() {
            label.to_owned()
        } else {
            format!("{symbol} {label}")
        }
    }

    /// Symbol-only controls still need a visible affordance on unknown fonts.
    pub fn play(self) -> &'static str {
        match self.symbol(Icon::Playing) {
            "" => ">",
            symbol => symbol,
        }
    }
}

pub fn current() -> Symbols {
    #[cfg(windows)]
    {
        Symbols::from_mask(crate::console::navigation_icon_mask())
    }
    #[cfg(not(windows))]
    {
        Symbols::NONE
    }
}

#[cfg(windows)]
pub const GLYPHS: [u16; 6] = [0x2302, 0x25b6, 0x25ba, 0x2630, 0x2261, 0x2315];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_glyphs_keep_labels_and_supported_alternatives_are_used() {
        for (icon, label) in [
            (Icon::Home, "Home"),
            (Icon::Playing, "Playing"),
            (Icon::Menu, "Menu"),
        ] {
            assert_eq!(Symbols::NONE.label(icon, label), label);
        }
        let common = Symbols::from_mask((1 << 0) | (1 << 2) | (1 << 4));
        assert_eq!(common.label(Icon::Home, "Home"), "⌂ Home");
        assert_eq!(common.label(Icon::Playing, "Playing"), "► Playing");
        assert_eq!(common.label(Icon::Menu, "Menu"), "≡ Menu");
        assert_eq!(common.symbol(Icon::Search), "");
        assert_eq!(Symbols::ALL.symbol(Icon::Playing), "▶");
        assert_eq!(Symbols::NONE.play(), ">");
        assert_eq!(common.play(), "►");
        assert_eq!(Symbols::ALL.play(), "▶");
    }
}
