//! OWL-606: every `toml` block of `docs/recipes.md` is a file Owlshift
//! accepts. The first project block is a whole project file; each later one is
//! a fragment, merged table by table onto it as a reader adds it to their own
//! file. A block marked `toml personal` is a personal file.

use std::path::Path;

use owlshift_contracts::config::{PersonalConfig, ProjectConfig};

struct Block {
    line: usize,
    personal: bool,
    text: String,
}

/// The fenced `toml` blocks of `text`, with the line each opens on.
fn blocks(text: &str) -> Vec<Block> {
    let mut found = Vec::new();
    let mut open: Option<Block> = None;
    for (i, line) in text.lines().enumerate() {
        match (&mut open, line.trim_end()) {
            (None, "```toml") => {
                open = Some(Block {
                    line: i + 1,
                    personal: false,
                    text: String::new(),
                });
            }
            (None, "```toml personal") => {
                open = Some(Block {
                    line: i + 1,
                    personal: true,
                    text: String::new(),
                });
            }
            (Some(_), "```") => found.extend(open.take()),
            (Some(block), _) => {
                block.text.push_str(line);
                block.text.push('\n');
            }
            (None, line) => assert!(
                !line.trim_start().starts_with("```toml"),
                "line {}: a toml fence this test does not read: {line:?}",
                i + 1
            ),
        }
    }
    assert!(open.is_none(), "a toml block is never closed");
    found
}

/// `fragment` laid over `base`: tables merge key by key, any other value
/// replaces.
fn merge(base: &mut toml::Table, fragment: toml::Table) {
    for (key, value) in fragment {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(into)), toml::Value::Table(from)) => merge(into, from),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[test]
fn every_toml_block_of_the_recipes_is_accepted() {
    let page = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/recipes.md");
    let text = std::fs::read_to_string(&page).unwrap();
    let blocks = blocks(&text);
    let mut project: Vec<&Block> = blocks.iter().filter(|b| !b.personal).collect();
    assert!(!project.is_empty(), "the page has no project file block");
    assert!(
        blocks.iter().any(|b| b.personal),
        "the page has no personal file block"
    );
    // Each personal block is a file of its own; read together, as a reader
    // who follows several recipes holds them, they are one file too.
    let mut together = toml::Table::new();
    let mut allowed: Vec<String> = Vec::new();
    for block in blocks.iter().filter(|b| b.personal) {
        let config = PersonalConfig::parse(&block.text)
            .unwrap_or_else(|error| panic!("line {}: {error}", block.line));
        allowed.extend(config.allow_caches);
        merge(&mut together, block.text.parse().unwrap());
    }
    PersonalConfig::parse(&toml::to_string(&together).unwrap())
        .unwrap_or_else(|error| panic!("the personal blocks together: {error}"));
    let base_block = project.remove(0);
    let base: toml::Table = base_block
        .text
        .parse()
        .unwrap_or_else(|error| panic!("line {}: {error}", base_block.line));
    ProjectConfig::parse(&base_block.text)
        .unwrap_or_else(|error| panic!("line {}: the base file: {error}", base_block.line));
    for block in project {
        let fragment: toml::Table = block
            .text
            .parse()
            .unwrap_or_else(|error| panic!("line {}: {error}", block.line));
        let mut file = base.clone();
        merge(&mut file, fragment);
        let file = toml::to_string(&file).unwrap();
        let config = ProjectConfig::parse(&file)
            .unwrap_or_else(|error| panic!("line {}: {error}\n{file}", block.line));
        // A cache the project declares is one a personal block allows.
        for folder in config.stack.cache_folders() {
            assert!(
                allowed.iter().any(|name| name == folder),
                "line {}: the cache {folder:?} is allowed by no personal block",
                block.line
            );
        }
    }
}

#[test]
fn the_merge_and_the_loaders_catch_what_the_page_must_not_hold() {
    let mut base: toml::Table = "[policy]\nmax_in_flight = 2\n".parse().unwrap();
    merge(&mut base, "[policy]\nmax_in_flight = 11\n".parse().unwrap());
    assert_eq!(base["policy"]["max_in_flight"].as_integer(), Some(11));
    let found = blocks("```toml\n[a]\n```\n```toml personal\nb = 1\n```\n");
    assert_eq!(found.len(), 2);
    assert!(found[1].personal);
    assert!(PersonalConfig::parse("concurrent_runs = 99\n").is_err());
}
