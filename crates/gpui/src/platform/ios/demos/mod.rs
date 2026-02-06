//! iOS demo modules for showcasing GPUI capabilities.
//!
//! These demos are designed to test and demonstrate various GPUI features
//! on iOS devices, including animations, shaders, and text input.

// Demo color palette (Catppuccin Mocha inspired)
pub mod colors {
    use crate::Hsla;

    pub const ROSEWATER: Hsla = Hsla {
        h: 10.0 / 360.0,
        s: 0.56,
        l: 0.91,
        a: 1.0,
    };
    pub const FLAMINGO: Hsla = Hsla {
        h: 0.0 / 360.0,
        s: 0.59,
        l: 0.88,
        a: 1.0,
    };
    pub const PINK: Hsla = Hsla {
        h: 316.0 / 360.0,
        s: 0.72,
        l: 0.86,
        a: 1.0,
    };
    pub const MAUVE: Hsla = Hsla {
        h: 267.0 / 360.0,
        s: 0.84,
        l: 0.81,
        a: 1.0,
    };
    pub const RED: Hsla = Hsla {
        h: 343.0 / 360.0,
        s: 0.81,
        l: 0.75,
        a: 1.0,
    };
    pub const MAROON: Hsla = Hsla {
        h: 350.0 / 360.0,
        s: 0.65,
        l: 0.77,
        a: 1.0,
    };
    pub const PEACH: Hsla = Hsla {
        h: 23.0 / 360.0,
        s: 0.92,
        l: 0.75,
        a: 1.0,
    };
    pub const YELLOW: Hsla = Hsla {
        h: 41.0 / 360.0,
        s: 0.86,
        l: 0.83,
        a: 1.0,
    };
    pub const GREEN: Hsla = Hsla {
        h: 115.0 / 360.0,
        s: 0.54,
        l: 0.76,
        a: 1.0,
    };
    pub const TEAL: Hsla = Hsla {
        h: 170.0 / 360.0,
        s: 0.57,
        l: 0.73,
        a: 1.0,
    };
    pub const SKY: Hsla = Hsla {
        h: 189.0 / 360.0,
        s: 0.71,
        l: 0.73,
        a: 1.0,
    };
    pub const SAPPHIRE: Hsla = Hsla {
        h: 199.0 / 360.0,
        s: 0.76,
        l: 0.69,
        a: 1.0,
    };
    pub const BLUE: Hsla = Hsla {
        h: 217.0 / 360.0,
        s: 0.92,
        l: 0.76,
        a: 1.0,
    };
    pub const LAVENDER: Hsla = Hsla {
        h: 232.0 / 360.0,
        s: 0.97,
        l: 0.85,
        a: 1.0,
    };

    // Base colors
    pub const TEXT: Hsla = Hsla {
        h: 227.0 / 360.0,
        s: 0.68,
        l: 0.88,
        a: 1.0,
    };
    pub const SUBTEXT1: Hsla = Hsla {
        h: 228.0 / 360.0,
        s: 0.39,
        l: 0.80,
        a: 1.0,
    };
    pub const SUBTEXT0: Hsla = Hsla {
        h: 227.0 / 360.0,
        s: 0.27,
        l: 0.72,
        a: 1.0,
    };
    pub const OVERLAY2: Hsla = Hsla {
        h: 228.0 / 360.0,
        s: 0.17,
        l: 0.64,
        a: 1.0,
    };
    pub const OVERLAY1: Hsla = Hsla {
        h: 227.0 / 360.0,
        s: 0.12,
        l: 0.56,
        a: 1.0,
    };
    pub const OVERLAY0: Hsla = Hsla {
        h: 228.0 / 360.0,
        s: 0.09,
        l: 0.49,
        a: 1.0,
    };
    pub const SURFACE2: Hsla = Hsla {
        h: 228.0 / 360.0,
        s: 0.10,
        l: 0.41,
        a: 1.0,
    };
    pub const SURFACE1: Hsla = Hsla {
        h: 227.0 / 360.0,
        s: 0.12,
        l: 0.33,
        a: 1.0,
    };
    pub const SURFACE0: Hsla = Hsla {
        h: 230.0 / 360.0,
        s: 0.14,
        l: 0.25,
        a: 1.0,
    };
    pub const BASE: Hsla = Hsla {
        h: 232.0 / 360.0,
        s: 0.23,
        l: 0.18,
        a: 1.0,
    };
    pub const MANTLE: Hsla = Hsla {
        h: 233.0 / 360.0,
        s: 0.23,
        l: 0.15,
        a: 1.0,
    };
    pub const CRUST: Hsla = Hsla {
        h: 232.0 / 360.0,
        s: 0.23,
        l: 0.12,
        a: 1.0,
    };
}

/// Available demo types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoType {
    Menu,
    AnimationPlayground,
    ShaderShowcase,
    TextEditor,
}

impl DemoType {
    pub fn name(&self) -> &'static str {
        match self {
            DemoType::Menu => "Demo Menu",
            DemoType::AnimationPlayground => "Animation Playground",
            DemoType::ShaderShowcase => "Shader Showcase",
            DemoType::TextEditor => "Text Editor",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            DemoType::Menu => "Select a demo to run",
            DemoType::AnimationPlayground => "Bouncing balls with physics simulation",
            DemoType::ShaderShowcase => "Gradient and particle effects",
            DemoType::TextEditor => "Touch-friendly text editing",
        }
    }

    pub fn all() -> &'static [DemoType] {
        &[
            DemoType::AnimationPlayground,
            DemoType::ShaderShowcase,
            DemoType::TextEditor,
        ]
    }
}
