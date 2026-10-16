//! Characters an agent can play: a name plus the persona that sets its voice.

use anyhow::{Context, bail};
use std::path::Path;
use werewolf_protocol::MAX_NAME_LEN;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Character {
    pub name: String,
    pub persona: Option<String>,
}

impl Character {
    pub fn new(name: &str, persona: Option<&str>) -> anyhow::Result<Self> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
            bail!("name {name:?} must be 1 to {MAX_NAME_LEN} characters");
        }
        Ok(Self {
            name: name.to_owned(),
            persona: persona
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned),
        })
    }
}

/// Reads a cast file of `Name|persona` lines; blank lines and `#` comments are skipped.
pub fn load(path: &Path) -> anyhow::Result<Vec<Character>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cast = parse(&text).with_context(|| format!("in cast file {}", path.display()))?;
    if cast.is_empty() {
        bail!("cast file {} has no characters", path.display());
    }
    Ok(cast)
}

fn parse(text: &str) -> anyhow::Result<Vec<Character>> {
    let mut cast: Vec<Character> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, persona) = line.split_once('|').unwrap_or((line, ""));
        let character =
            Character::new(name, Some(persona)).with_context(|| format!("line {}", i + 1))?;
        // The server rejects duplicate names, so a repeat could never be played.
        if cast
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(&character.name))
        {
            bail!("line {}: {} appears twice", i + 1, character.name);
        }
        cast.push(character);
    }
    Ok(cast)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_personas_and_skips_comments() {
        let cast =
            parse("# the usual suspects\nSherlock|Cold and precise.\n\n  Watson  \nMarple| \n")
                .unwrap();
        assert_eq!(
            cast,
            [
                Character::new("Sherlock", Some("Cold and precise.")).unwrap(),
                Character::new("Watson", None).unwrap(),
                Character::new("Marple", None).unwrap(),
            ]
        );
    }

    #[test]
    fn rejects_bad_names_and_duplicates() {
        let long = "x".repeat(MAX_NAME_LEN + 1);
        assert!(parse(&format!("{long}|too long")).is_err());
        assert!(parse("|no name").is_err());
        let err = parse("Ada|one\nada|two").unwrap_err();
        assert!(
            format!("{err:#}").contains("line 2: ada appears twice"),
            "{err:#}"
        );
    }
}
