use clap::ValueEnum;

/// Token counters for a single API call, or an aggregate of many.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tokens([u64; 4]);

impl Tokens {
    pub fn new(input: u64, output: u64, cache_write: u64, cache_read: u64) -> Self {
        Self([input, output, cache_write, cache_read])
    }

    pub fn get(&self, category: Category) -> u64 {
        self.0[category as usize]
    }

    /// Tokens occupying the context window for this call: everything sent as input.
    pub fn context(&self) -> u64 {
        self.get(Category::Input) + self.get(Category::CacheWrite) + self.get(Category::CacheRead)
    }
}

impl std::ops::AddAssign for Tokens {
    fn add_assign(&mut self, rhs: Self) {
        self.0.iter_mut().zip(rhs.0).for_each(|(a, b)| *a += b);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Category {
    #[value(alias = "in")]
    Input = 0,
    #[value(alias = "out")]
    Output = 1,
    #[value(name = "cache-write", alias = "cw")]
    CacheWrite = 2,
    #[value(name = "cache-read", alias = "cr")]
    CacheRead = 3,
}

impl Category {
    pub const ALL: [Category; 4] = [
        Category::Input,
        Category::Output,
        Category::CacheWrite,
        Category::CacheRead,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Input => "input",
            Category::Output => "output",
            Category::CacheWrite => "cache write",
            Category::CacheRead => "cache read",
        }
    }

    pub fn color(self) -> Rgb {
        match self {
            Category::Input => gruvbox::BLUE,
            Category::Output => gruvbox::ORANGE,
            Category::CacheWrite => gruvbox::YELLOW,
            Category::CacheRead => gruvbox::AQUA,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
}

pub mod gruvbox {
    use super::Rgb;

    pub const BG: Rgb = Rgb(0x28, 0x28, 0x28);
    pub const BG1: Rgb = Rgb(0x3c, 0x38, 0x36);
    pub const FG: Rgb = Rgb(0xeb, 0xdb, 0xb2);
    pub const FG4: Rgb = Rgb(0xa8, 0x99, 0x84);
    pub const GRAY: Rgb = Rgb(0x92, 0x83, 0x74);
    pub const RED: Rgb = Rgb(0xfb, 0x49, 0x34);
    pub const GREEN: Rgb = Rgb(0xb8, 0xbb, 0x26);
    pub const YELLOW: Rgb = Rgb(0xfa, 0xbd, 0x2f);
    pub const BLUE: Rgb = Rgb(0x83, 0xa5, 0x98);
    pub const PURPLE: Rgb = Rgb(0xd3, 0x86, 0x9b);
    pub const AQUA: Rgb = Rgb(0x8e, 0xc0, 0x7c);
    pub const ORANGE: Rgb = Rgb(0xfe, 0x80, 0x19);
}

/// `1234567` -> `1.2M`
pub fn human(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "G"), (1_000_000, "M"), (1_000, "k")];
    for (scale, suffix) in UNITS {
        if n >= scale {
            let v = n as f64 / scale as f64;
            return if v < 10.0 {
                format!("{v:.1}{suffix}")
            } else {
                format!("{v:.0}{suffix}")
            };
        }
    }
    n.to_string()
}

/// `claude-opus-5-5-20260101` -> `opus-5-5`
pub fn short_model(model: &str) -> String {
    let m = model.strip_prefix("claude-").unwrap_or(model);
    let m = match m.rsplit_once('-') {
        Some((head, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => m,
    };
    m.to_string()
}
