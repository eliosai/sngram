/*!
Provides completions for ripgrep's CLI for the zsh shell.

Unlike completion short for other shells (at time of writing), zsh's
completions for ripgrep are maintained by hand. This is because:

1. They are lovingly written by an expert in such things.
2. Are much higher in quality than the ones below that are auto-generated.
Namely, the zsh completions take application level context about flag
compatibility into account.
3. There is a CI script that fails if a new flag is added to ripgrep that
isn't included in the zsh completions.
4. There is a wealth of documentation in the zsh script explaining how it
works and how it can be extended.

In principle, I'd be open to maintaining any completion script by hand so
long as it meets criteria 3 and 4 above.
*/

/// Generate completions for zsh.
pub(crate) fn generate() -> String {
    let hyperlink_alias_descriptions = grep::printer::hyperlink_aliases()
        .iter()
        .map(|alias| format!(r#"    {}:"{}""#, alias.name(), alias.description()))
        .collect::<Vec<String>>()
        .join("\n");
    include_str!("rg.zsh")
        .replace("#compdef rg", "#compdef eg")
        .replace("zsh completion function for ripgrep", "zsh completion function for elgrep")
        .replace("`rg` binary", "`eg` binary")
        .replace("`rg` are", "`eg` are")
        .replace("Completion script for ripgrep", "Completion script for elgrep")
        .replace("ripgrep has many options", "elgrep has many options")
        .replace("_rg() {", "_eg() {")
        .replace("[[ $funcstack[1] == _rg ]]", "[[ $funcstack[1] == _eg ]]")
        .replace("_rg \"$@\"", "_eg \"$@\"")
        .replace("compdef _rg rg", "compdef _eg eg")
        .replace(
            "    '(: * -)'--pcre2-version'[print the version of PCRE2 used by ripgrep, if available]'",
            "    '(: * -)'--pcre2-version'[print the version of PCRE2 used by elgrep, if available]'\n    '--index=[use sparse n-gram indexed search]:index mode:(auto rebuild)'\n    '--index-backend=[select sparse n-gram index backend]:index backend:(postings tantivy tantivy-ram)'\n    '--no-index[disable sparse n-gram indexed search]'",
        )
        .replace("!ENCODINGS!", super::ENCODINGS.trim_end())
        .replace("!HYPERLINK_ALIASES!", &hyperlink_alias_descriptions)
}
