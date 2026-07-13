//! Language detection: file-extension → language-kind mapping.

use std::path::Path;

pub fn detect_language(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    const EXT_MAP: &[(&str, &str)] = &[
        ("rs", "rust"),
        ("py", "python"),
        ("js", "javascript"),
        ("mjs", "javascript"),
        ("cjs", "javascript"),
        ("ts", "typescript"),
        ("mts", "typescript"),
        ("tsx", "tsx"),
        ("jsx", "jsx"),
        ("go", "go"),
        ("java", "java"),
        ("cpp", "cpp"),
        ("cc", "cpp"),
        ("cxx", "cpp"),
        ("c", "c"),
        ("h", "cpp"),
        ("hpp", "cpp"),
        ("cs", "csharp"),
        ("rb", "ruby"),
        ("kt", "kotlin"),
        ("kts", "kotlin"),
        ("swift", "swift"),
        ("md", "markdown"),
        ("markdown", "markdown"),
        ("toml", "toml"),
        ("json", "json"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
        ("html", "html"),
        ("htm", "html"),
        ("css", "css"),
        ("scss", "css"),
        ("sass", "css"),
        ("sh", "shell"),
        ("bash", "shell"),
        ("sql", "sql"),
        ("php", "php"),
        ("lua", "lua"),
        ("zig", "zig"),
        ("cbl", "cobol"),
        ("cob", "cobol"),
        ("cpy", "cobol"),
        ("cobol", "cobol"),
        ("jcl", "jcl"),
        ("nsp", "natural"),
        ("nse", "natural"),
        ("nsd", "natural"),
        ("nsl", "natural"),
        ("nst", "natural"),
        ("nsn", "natural"),
        ("rpg", "rpg"),
        ("rpgle", "rpg"),
        ("sqlrpgle", "rpg"),
        ("sru", "powerscript"),
        ("sra", "powerscript"),
        ("srd", "powerscript"),
        ("srw", "powerscript"),
        ("pbl", "powerscript"),
        ("srf", "powerscript"),
    ];

    EXT_MAP
        .iter()
        .find(|&&(e, _)| e.eq_ignore_ascii_case(ext))
        .map(|&(_, lang)| lang)
        .unwrap_or("text")
}
