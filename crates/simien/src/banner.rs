//! The Simien mark, as the TypeScript CLI draws it: shown only with --human
//! on a terminal; JSON output never carries it.

use std::io::IsTerminal;

const INDENT: &str = "  ";

/// "simien" in the Ogre FIGlet font (figlet 1.8.0).
pub const LOGO: [&str; 5] = [
    "     _           _",
    " ___(_)_ __ ___ (_) ___ _ __",
    "/ __| | '_ ` _ \\| |/ _ \\ '_ \\",
    "\\__ \\ | | | | | | |  __/ | | |",
    "|___/_|_| |_| |_|_|\\___|_| |_|",
];

/// Simien blue.
const MAIN: (u8, u8, u8) = (94, 176, 255);

/// Whether to draw: --human, with a terminal on stdout.
pub fn show(human: bool) -> bool {
    human && std::io::stdout().is_terminal()
}

pub fn logo(color: bool) -> String {
    LOGO.iter()
        .map(|row| {
            if color {
                let (r, g, b) = MAIN;
                format!("{INDENT}\x1b[38;2;{r};{g};{b}m{row}\x1b[39m")
            } else {
                format!("{INDENT}{row}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
