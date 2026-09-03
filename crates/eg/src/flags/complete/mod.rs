/*!
Modules for generating completions for various shells.
*/

static ENCODINGS: &'static str = include_str!("encodings.sh");

pub mod bash;
pub mod fish;
pub mod powershell;
pub mod zsh;
