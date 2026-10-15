//! Lobby settings the host can change between games from the UI.

use std::{num::NonZeroUsize, ops::RangeInclusive, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{
    config::Config,
    engine::{Setup, describe_setup, players_needed},
};

/// Settings in force for the next game. CLI values may be fractional, so times stay exact here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub setup: Setup,
    pub discussion: Duration,
    pub ai_chat_cooldown: Duration,
    pub ai_max_chat_messages: u32,
    pub human_turn_timeout: Duration,
}

/// Wire form for `/admin/settings`, snapshots, and `/v1/info`: whole seconds.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    pub wolves: Option<u32>,
    pub sheriff: bool,
    pub cupid: bool,
    pub discussion_secs: u32,
    pub ai_chat_cooldown_secs: u32,
    pub ai_max_chat_messages: u32,
    pub human_turn_timeout_secs: u32,
}

const WOLVES: RangeInclusive<u32> = 1..=3;
const DISCUSSION_SECS: RangeInclusive<u32> = 30..=600;
const AI_CHAT_COOLDOWN_SECS: RangeInclusive<u32> = 1..=120;
const AI_MAX_CHAT_MESSAGES: RangeInclusive<u32> = 1..=20;
const HUMAN_TURN_TIMEOUT_SECS: RangeInclusive<u32> = 10..=300;

impl Settings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            setup: Setup {
                wolves: config.wolves,
                sheriff: config.sheriff(),
                cupid: config.cupid(),
            },
            discussion: config.discussion_secs,
            ai_chat_cooldown: config.ai_chat_cooldown_secs,
            ai_max_chat_messages: config.ai_max_chat_messages,
            human_turn_timeout: Duration::from_secs(config.human_turn_timeout_secs),
        }
    }

    pub fn body(&self) -> SettingsBody {
        let secs = |d: Duration| d.as_secs_f64().round() as u32;
        SettingsBody {
            wolves: self
                .setup
                .wolves
                .map(|w| u32::try_from(w.get()).unwrap_or(u32::MAX)),
            sheriff: self.setup.sheriff,
            cupid: self.setup.cupid,
            discussion_secs: secs(self.discussion),
            ai_chat_cooldown_secs: secs(self.ai_chat_cooldown),
            ai_max_chat_messages: self.ai_max_chat_messages,
            human_turn_timeout_secs: secs(self.human_turn_timeout),
        }
    }
}

impl SettingsBody {
    /// Checks every field's range and that `max_players` seats can hold the setup.
    pub fn validate(self, max_players: usize) -> Result<Settings, String> {
        let check = |name: &str, value: u32, range: RangeInclusive<u32>| {
            if range.contains(&value) {
                Ok(value)
            } else {
                Err(format!(
                    "{name} must be {}-{}, got {value}",
                    range.start(),
                    range.end()
                ))
            }
        };
        if let Some(wolves) = self.wolves
            && !WOLVES.contains(&wolves)
        {
            return Err(format!("wolves must be null or 1-3, got {wolves}"));
        }
        let discussion = check("discussion_secs", self.discussion_secs, DISCUSSION_SECS)?;
        let cooldown = check(
            "ai_chat_cooldown_secs",
            self.ai_chat_cooldown_secs,
            AI_CHAT_COOLDOWN_SECS,
        )?;
        let messages = check(
            "ai_max_chat_messages",
            self.ai_max_chat_messages,
            AI_MAX_CHAT_MESSAGES,
        )?;
        let timeout = check(
            "human_turn_timeout_secs",
            self.human_turn_timeout_secs,
            HUMAN_TURN_TIMEOUT_SECS,
        )?;
        let wolves = self.wolves.and_then(|w| NonZeroUsize::new(w as usize));
        if let Some(wolves) = wolves {
            let need = players_needed(wolves.get(), self.sheriff, self.cupid);
            if need > max_players {
                let setup = describe_setup(wolves.get(), self.sheriff, self.cupid);
                return Err(format!(
                    "{setup} need {need} players, but --max-players is {max_players}"
                ));
            }
        }
        Ok(Settings {
            setup: Setup {
                wolves,
                sheriff: self.sheriff,
                cupid: self.cupid,
            },
            discussion: Duration::from_secs(discussion.into()),
            ai_chat_cooldown: Duration::from_secs(cooldown.into()),
            ai_max_chat_messages: messages,
            human_turn_timeout: Duration::from_secs(timeout.into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: SettingsBody = SettingsBody {
        wolves: Some(2),
        sheriff: true,
        cupid: true,
        discussion_secs: 90,
        ai_chat_cooldown_secs: 20,
        ai_max_chat_messages: 4,
        human_turn_timeout_secs: 120,
    };

    #[test]
    fn round_trips_valid_settings() {
        let settings = VALID.validate(12).unwrap();
        assert_eq!(settings.body(), VALID);
        assert_eq!(settings.setup.wolves, NonZeroUsize::new(2));
    }

    #[test]
    fn rejects_out_of_range_fields() {
        let cases = [
            (
                SettingsBody {
                    wolves: Some(0),
                    ..VALID
                },
                "wolves",
            ),
            (
                SettingsBody {
                    wolves: Some(4),
                    ..VALID
                },
                "wolves",
            ),
            (
                SettingsBody {
                    discussion_secs: 29,
                    ..VALID
                },
                "discussion_secs",
            ),
            (
                SettingsBody {
                    discussion_secs: 601,
                    ..VALID
                },
                "discussion_secs",
            ),
            (
                SettingsBody {
                    ai_chat_cooldown_secs: 0,
                    ..VALID
                },
                "ai_chat_cooldown_secs",
            ),
            (
                SettingsBody {
                    ai_max_chat_messages: 21,
                    ..VALID
                },
                "ai_max_chat_messages",
            ),
            (
                SettingsBody {
                    human_turn_timeout_secs: 9,
                    ..VALID
                },
                "human_turn_timeout_secs",
            ),
        ];
        for (body, field) in cases {
            let err = body.validate(12).unwrap_err();
            assert!(err.starts_with(field), "{err}");
        }
        assert!(
            SettingsBody {
                wolves: None,
                ..VALID
            }
            .validate(12)
            .is_ok()
        );
        let err = |wolves, sheriff, cupid, max_players| {
            SettingsBody {
                wolves: Some(wolves),
                sheriff,
                cupid,
                ..VALID
            }
            .validate(max_players)
            .err()
        };
        assert_eq!(
            err(3, true, false, 6).as_deref(),
            Some("3 wolves + sheriff need 7 players, but --max-players is 6")
        );
        assert_eq!(
            err(3, true, true, 7).as_deref(),
            Some("3 wolves + sheriff + cupid need 8 players, but --max-players is 7")
        );
        assert_eq!(
            err(1, true, true, 5).as_deref(),
            Some("1 wolves + sheriff + cupid need 6 players, but --max-players is 5")
        );
        assert_eq!(err(1, false, true, 5), None);
        assert_eq!(err(3, true, true, 8), None);
    }
}
