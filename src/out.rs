//! Terminal output: the same colours, symbols and columns as the bash helpers.

use std::io::{IsTerminal, Write};

/// An error that ends the command: printed by `die`, exit code 1.
#[derive(Debug)]
pub struct Die(pub String);

pub type R<T> = Result<T, Die>;

pub fn die<T>(msg: impl Into<String>) -> R<T> {
    Err(Die(msg.into()))
}

/// Prints a `Die` the way bash `die` does, on stderr.
pub fn print_die(d: &Die) {
    let colour = std::io::stderr().is_terminal() && !env_set("NO_COLOR");
    if colour {
        eprintln!("\x1b[31m✗\x1b[0m {}", d.0);
    } else {
        eprintln!("error: {}", d.0);
    }
}

fn env_set(name: &str) -> bool {
    std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Ok,
    Warn,
    Bad,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Colour {
    Red,
    Grn,
    Yel,
    Blu,
    Dim,
}

/// Output settings: colour on a terminal only, off with NO_COLOR or TERM=dumb.
pub struct Out {
    pub tty: bool,
}

impl Out {
    pub fn detect() -> Out {
        let dumb = std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
        Out {
            tty: std::io::stdout().is_terminal() && !env_set("NO_COLOR") && !dumb,
        }
    }

    pub fn plain() -> Out {
        Out { tty: false }
    }

    fn code(&self, c: &'static str) -> &'static str {
        if self.tty {
            c
        } else {
            ""
        }
    }

    pub fn red(&self) -> &'static str {
        self.code("\x1b[31m")
    }
    pub fn grn(&self) -> &'static str {
        self.code("\x1b[32m")
    }
    pub fn yel(&self) -> &'static str {
        self.code("\x1b[33m")
    }
    pub fn blu(&self) -> &'static str {
        self.code("\x1b[36m")
    }
    pub fn dim(&self) -> &'static str {
        self.code("\x1b[2m")
    }
    pub fn bold(&self) -> &'static str {
        self.code("\x1b[1m")
    }
    pub fn reset(&self) -> &'static str {
        self.code("\x1b[0m")
    }

    fn colour(&self, c: Colour) -> &'static str {
        match c {
            Colour::Red => self.red(),
            Colour::Grn => self.grn(),
            Colour::Yel => self.yel(),
            Colour::Blu => self.blu(),
            Colour::Dim => self.dim(),
        }
    }

    /// `done_`: a green tick on a terminal, the bare text otherwise.
    pub fn done(&self, w: &mut dyn Write, msg: &str) {
        if self.tty {
            let _ = writeln!(w, "{}✓{} {}", self.grn(), self.reset(), msg);
        } else {
            let _ = writeln!(w, "{msg}");
        }
    }

    /// `step`: a blue arrow on a terminal, the bare text otherwise.
    pub fn step(&self, w: &mut dyn Write, msg: &str) {
        if self.tty {
            let _ = writeln!(w, "{}›{} {}", self.blu(), self.reset(), msg);
        } else {
            let _ = writeln!(w, "{msg}");
        }
    }

    /// `hint`: indented, dim on a terminal.
    pub fn hint(&self, w: &mut dyn Write, msg: &str) {
        if self.tty {
            let _ = writeln!(w, "  {}{}{}", self.dim(), msg, self.reset());
        } else {
            let _ = writeln!(w, "  {msg}");
        }
    }

    /// `warn`: `!` on a terminal, `warning:` prefix otherwise.
    pub fn warn(&self, w: &mut dyn Write, msg: &str) {
        if self.tty {
            let _ = writeln!(w, "{}!{} {}", self.yel(), self.reset(), msg);
        } else {
            let _ = writeln!(w, "warning: {msg}");
        }
    }

    /// One line of `check`: symbol and colour on a terminal, `ok|warn|bad` otherwise.
    pub fn item(&self, w: &mut dyn Write, kind: Kind, label: &str, text: &str) {
        let (sym, col, word) = match kind {
            Kind::Ok => ("✓", self.grn(), "ok"),
            Kind::Warn => ("!", self.yel(), "warn"),
            Kind::Bad => ("✗", self.red(), "bad"),
        };
        if self.tty {
            let _ = writeln!(w, "{}{}{} {:<9} {}", col, sym, self.reset(), label, text);
        } else {
            let _ = writeln!(w, "{word:<4} {label:<9} {text}");
        }
    }

    /// The text padded to `width`, coloured when it equals one of the values (the last match wins).
    pub fn paint(&self, text: &str, width: usize, map: &[(&str, Colour)]) -> String {
        let mut c = "";
        for (value, colour) in map {
            if *value == text {
                c = self.colour(*colour);
            }
        }
        let reset = if c.is_empty() { "" } else { self.reset() };
        format!("{c}{text:<width$}{reset}")
    }
}
