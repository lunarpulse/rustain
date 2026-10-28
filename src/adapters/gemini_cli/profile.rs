//! Per-model Gemini CLI wire profile, adapted from oh-my-pi (catalog/src/models.json,
//! catalog/src/compat/rules/classes/gemini.kdl, catalog/src/compat/rules/providers/google-gemini-cli.kdl).
//!
//! MIT License
//!
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

/// omp thinking effort tiers a Gemini CLI row can expose, in canonical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effort {
    Minimal,
    Low,
    Medium,
    High,
}

impl Effort {
    /// omp `THINKING_EFFORTS` order (the subset Gemini rows use).
    pub(crate) const CANONICAL: [Effort; 4] =
        [Effort::Minimal, Effort::Low, Effort::Medium, Effort::High];
}

const ALL_EFFORTS: &[Effort] = &Effort::CANONICAL;
const LOW_HIGH: &[Effort] = &[Effort::Low, Effort::High];
/// Budget rows carry the pre-3 four-tier ladder (`gemini.kdl` `revision "<3"`).
pub(crate) const BUDGET_EFFORTS: &[Effort] = ALL_EFFORTS;

/// omp `thinking.mode` with its mandatory-effort flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Thinking {
    None,
    /// `budget`: `thinkingBudget` tokens, efforts minimal…high.
    Budget {
        requires_effort: bool,
    },
    /// `google-level`: `thinkingLevel`.
    Level {
        efforts: &'static [Effort],
        requires_effort: bool,
    },
}

impl Thinking {
    /// Supported efforts, canonical order.
    pub(crate) fn efforts(self) -> &'static [Effort] {
        match self {
            Thinking::None => &[],
            Thinking::Budget { .. } => BUDGET_EFFORTS,
            Thinking::Level { efforts, .. } => efforts,
        }
    }

    pub(crate) fn requires_effort(self) -> bool {
        match self {
            Thinking::None => false,
            Thinking::Budget { requires_effort }
            | Thinking::Level {
                requires_effort, ..
            } => requires_effort,
        }
    }
}

/// omp `thinking.effortRouting`: wire model id per effort (`off` = thinking off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortRoutes {
    pub off: Option<&'static str>,
    pub minimal: Option<&'static str>,
    pub low: Option<&'static str>,
    pub medium: Option<&'static str>,
    pub high: Option<&'static str>,
}

impl EffortRoutes {
    /// Route for `effort` (`None` = off).
    pub(crate) fn route(&self, effort: Option<Effort>) -> Option<&'static str> {
        match effort {
            None => self.off,
            Some(Effort::Minimal) => self.minimal,
            Some(Effort::Low) => self.low,
            Some(Effort::Medium) => self.medium,
            Some(Effort::High) => self.high,
        }
    }
}

/// Wire-relevant facts of one Gemini CLI model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GeminiModelProfile {
    pub reasoning: bool,
    pub thinking: Thinking,
    pub max_output: u32,
    pub vision: bool,
    /// `requiresSkipThoughtSignatureOnFirstFunctionCall`.
    pub first_call_sentinel: bool,
    /// `flashStreamLeakWorkaround`.
    pub flash_leak: bool,
    /// `streamFirstEventTimeoutMs ?? 300000`.
    pub first_event_ms: u64,
    pub routes: Option<EffortRoutes>,
    pub suppress_when_off: bool,
    /// `multimodalFunctionResponse`.
    pub multimodal_function_response: bool,
}

const DEFAULT_MAX_OUTPUT: u32 = 65_536;
const FLASH_FIRST_EVENT_MS: u64 = 60_000;
const DEFAULT_FIRST_EVENT_MS: u64 = 300_000;

const UNKNOWN: GeminiModelProfile = GeminiModelProfile {
    reasoning: false,
    thinking: Thinking::None,
    max_output: DEFAULT_MAX_OUTPUT,
    vision: true,
    first_call_sentinel: false,
    flash_leak: false,
    first_event_ms: DEFAULT_FIRST_EVENT_MS,
    routes: None,
    suppress_when_off: false,
    multimodal_function_response: false,
};

/// Bundled `google-gemini-cli` rows of omp `models.json`, in omp order.
const BUNDLED: [(&str, GeminiModelProfile); 7] = [
    (
        "gemini-2.0-flash",
        GeminiModelProfile {
            reasoning: false,
            thinking: Thinking::None,
            max_output: 8_192,
            vision: true,
            first_call_sentinel: false,
            flash_leak: true,
            first_event_ms: FLASH_FIRST_EVENT_MS,
            routes: None,
            suppress_when_off: false,
            multimodal_function_response: false,
        },
    ),
    (
        "gemini-2.5-flash",
        GeminiModelProfile {
            reasoning: true,
            thinking: Thinking::Budget {
                requires_effort: false,
            },
            max_output: 65_536,
            vision: true,
            first_call_sentinel: false,
            flash_leak: true,
            first_event_ms: FLASH_FIRST_EVENT_MS,
            routes: Some(EffortRoutes {
                off: Some("gemini-2.5-flash"),
                minimal: None,
                low: None,
                medium: None,
                high: None,
            }),
            suppress_when_off: false,
            multimodal_function_response: false,
        },
    ),
    (
        "gemini-2.5-pro",
        GeminiModelProfile {
            reasoning: true,
            thinking: Thinking::Budget {
                requires_effort: true,
            },
            max_output: 65_536,
            vision: true,
            first_call_sentinel: false,
            flash_leak: false,
            first_event_ms: DEFAULT_FIRST_EVENT_MS,
            routes: None,
            suppress_when_off: false,
            multimodal_function_response: false,
        },
    ),
    (
        "gemini-3-flash-preview",
        GeminiModelProfile {
            reasoning: true,
            thinking: Thinking::Level {
                efforts: ALL_EFFORTS,
                requires_effort: true,
            },
            max_output: 65_536,
            vision: true,
            first_call_sentinel: true,
            flash_leak: true,
            first_event_ms: FLASH_FIRST_EVENT_MS,
            routes: None,
            suppress_when_off: false,
            multimodal_function_response: true,
        },
    ),
    (
        "gemini-3-pro-preview",
        GeminiModelProfile {
            reasoning: true,
            thinking: Thinking::Level {
                efforts: LOW_HIGH,
                requires_effort: true,
            },
            max_output: 64_000,
            vision: true,
            first_call_sentinel: true,
            flash_leak: false,
            first_event_ms: DEFAULT_FIRST_EVENT_MS,
            routes: None,
            suppress_when_off: false,
            multimodal_function_response: true,
        },
    ),
    (
        "gemini-3.1-flash-lite-preview",
        GeminiModelProfile {
            reasoning: true,
            thinking: Thinking::Level {
                efforts: ALL_EFFORTS,
                requires_effort: true,
            },
            max_output: 65_536,
            vision: true,
            first_call_sentinel: true,
            flash_leak: false,
            first_event_ms: DEFAULT_FIRST_EVENT_MS,
            routes: None,
            suppress_when_off: false,
            multimodal_function_response: true,
        },
    ),
    (
        "gemini-3.1-pro-preview",
        GeminiModelProfile {
            reasoning: true,
            thinking: Thinking::Level {
                efforts: LOW_HIGH,
                requires_effort: true,
            },
            max_output: 65_536,
            vision: true,
            first_call_sentinel: true,
            flash_leak: false,
            first_event_ms: DEFAULT_FIRST_EVENT_MS,
            routes: None,
            suppress_when_off: false,
            multimodal_function_response: true,
        },
    ),
];

/// Profile of `id`: a bundled row, else omp's class rules for the parsed id.
pub(crate) fn for_model(id: &str) -> GeminiModelProfile {
    if let Some((_, profile)) = BUNDLED.iter().find(|(bundled, _)| *bundled == id) {
        return *profile;
    }
    derive(id)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Pro,
    Flash,
    Lite,
    Other,
}

/// `gemini-<major>[.<minor>[.<patch>]]-<family>…` → revision and family.
fn parse(id: &str) -> Option<((u32, u32, u32), Family)> {
    let rest = id.strip_prefix("gemini-")?;
    let (version, tail) = rest.split_once('-').unwrap_or((rest, ""));
    let mut numbers = version.split('.');
    let mut component = || -> Option<Option<u32>> {
        match numbers.next() {
            None => Some(None),
            Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                text.parse().ok().map(Some)
            }
            Some(_) => None,
        }
    };
    let major = component()??;
    let minor = component()?.unwrap_or(0);
    let patch = component()?.unwrap_or(0);
    if numbers.next().is_some() {
        return None;
    }
    let mut tokens = tail.split('-');
    let family = match tokens.next() {
        Some("pro") => Family::Pro,
        Some("flash") if tokens.next() == Some("lite") => Family::Lite,
        Some("flash") => Family::Flash,
        Some("lite") => Family::Lite,
        _ => Family::Other,
    };
    Some(((major, minor, patch), family))
}

fn derive(id: &str) -> GeminiModelProfile {
    let Some((revision, family)) = parse(id) else {
        return UNKNOWN;
    };
    let reasoning = revision >= (2, 5, 0);
    let gemini3 = revision >= (3, 0, 0);
    let thinking = if !reasoning {
        Thinking::None
    } else if gemini3 {
        Thinking::Level {
            efforts: if family == Family::Pro {
                LOW_HIGH
            } else {
                ALL_EFFORTS
            },
            requires_effort: true,
        }
    } else {
        Thinking::Budget {
            requires_effort: family == Family::Pro,
        }
    };
    let flash = family == Family::Flash;
    GeminiModelProfile {
        reasoning,
        thinking,
        max_output: DEFAULT_MAX_OUTPUT,
        vision: true,
        first_call_sentinel: gemini3,
        flash_leak: flash,
        first_event_ms: if flash {
            FLASH_FIRST_EVENT_MS
        } else {
            DEFAULT_FIRST_EVENT_MS
        },
        routes: None,
        suppress_when_off: false,
        multimodal_function_response: gemini3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(efforts: &'static [Effort]) -> Thinking {
        Thinking::Level {
            efforts,
            requires_effort: true,
        }
    }

    #[test]
    fn bundled_rows_match_the_catalogue() {
        // (id, reasoning, thinking, max out, sentinel, leak, first event, mfr)
        let rows: [(&str, bool, Thinking, u32, bool, bool, u64, bool); 7] = [
            (
                "gemini-2.0-flash",
                false,
                Thinking::None,
                8_192,
                false,
                true,
                60_000,
                false,
            ),
            (
                "gemini-2.5-flash",
                true,
                Thinking::Budget {
                    requires_effort: false,
                },
                65_536,
                false,
                true,
                60_000,
                false,
            ),
            (
                "gemini-2.5-pro",
                true,
                Thinking::Budget {
                    requires_effort: true,
                },
                65_536,
                false,
                false,
                300_000,
                false,
            ),
            (
                "gemini-3-flash-preview",
                true,
                level(ALL_EFFORTS),
                65_536,
                true,
                true,
                60_000,
                true,
            ),
            (
                "gemini-3-pro-preview",
                true,
                level(LOW_HIGH),
                64_000,
                true,
                false,
                300_000,
                true,
            ),
            (
                "gemini-3.1-flash-lite-preview",
                true,
                level(ALL_EFFORTS),
                65_536,
                true,
                false,
                300_000,
                true,
            ),
            (
                "gemini-3.1-pro-preview",
                true,
                level(LOW_HIGH),
                65_536,
                true,
                false,
                300_000,
                true,
            ),
        ];
        for (id, reasoning, thinking, max_output, sentinel, leak, first_event, mfr) in rows {
            let profile = for_model(id);
            assert_eq!(profile.reasoning, reasoning, "{id}");
            assert_eq!(profile.thinking, thinking, "{id}");
            assert_eq!(profile.max_output, max_output, "{id}");
            assert!(profile.vision, "{id}");
            assert_eq!(profile.first_call_sentinel, sentinel, "{id}");
            assert_eq!(profile.flash_leak, leak, "{id}");
            assert_eq!(profile.first_event_ms, first_event, "{id}");
            assert_eq!(profile.multimodal_function_response, mfr, "{id}");
            assert!(!profile.suppress_when_off, "{id}");
            let expected_routes = (id == "gemini-2.5-flash").then_some(EffortRoutes {
                off: Some("gemini-2.5-flash"),
                minimal: None,
                low: None,
                medium: None,
                high: None,
            });
            assert_eq!(profile.routes, expected_routes, "{id}");
        }
    }

    #[test]
    fn derived_ids_follow_the_class_rules() {
        let flash = for_model("gemini-3.6-flash");
        assert_eq!(
            flash,
            GeminiModelProfile {
                reasoning: true,
                thinking: level(ALL_EFFORTS),
                max_output: 65_536,
                vision: true,
                first_call_sentinel: true,
                flash_leak: true,
                first_event_ms: 60_000,
                routes: None,
                suppress_when_off: false,
                multimodal_function_response: true,
            }
        );

        let pro4 = for_model("gemini-4-pro");
        assert_eq!(pro4.thinking, level(LOW_HIGH));
        assert!(pro4.reasoning && pro4.first_call_sentinel && pro4.multimodal_function_response);
        assert!(!pro4.flash_leak);
        assert_eq!((pro4.first_event_ms, pro4.max_output), (300_000, 65_536));

        let old_pro = for_model("gemini-2.0-pro");
        assert_eq!(old_pro, UNKNOWN);

        let lite = for_model("gemini-3.1-flash-lite-x");
        assert_eq!(lite.thinking, level(ALL_EFFORTS));
        assert!(lite.first_call_sentinel && lite.multimodal_function_response);
        assert!(!lite.flash_leak);
        assert_eq!(lite.first_event_ms, 300_000);

        let budget_pro = for_model("gemini-2.7-pro-exp");
        assert_eq!(
            budget_pro.thinking,
            Thinking::Budget {
                requires_effort: true
            }
        );
        assert!(!budget_pro.first_call_sentinel && !budget_pro.multimodal_function_response);

        let budget_flash = for_model("gemini-2.5.1-flash-exp");
        assert_eq!(
            budget_flash.thinking,
            Thinking::Budget {
                requires_effort: false
            }
        );
        assert!(budget_flash.flash_leak);
        assert_eq!(budget_flash.first_event_ms, 60_000);

        for unparsable in [
            "claude-sonnet-4",
            "gemini-x-pro",
            "gemini-",
            "gemini-3.a-pro",
        ] {
            let profile = for_model(unparsable);
            assert_eq!(profile, UNKNOWN, "{unparsable}");
            assert!(profile.vision && !profile.reasoning && !profile.first_call_sentinel);
            assert_eq!(
                (profile.first_event_ms, profile.max_output),
                (300_000, 65_536)
            );
        }
    }

    #[test]
    fn routes_resolve_per_effort() {
        let routes = for_model("gemini-2.5-flash")
            .routes
            .expect("bundled off route");
        assert_eq!(routes.route(None), Some("gemini-2.5-flash"));
        assert_eq!(routes.route(Some(Effort::Minimal)), None);
        assert_eq!(Thinking::None.efforts(), &[] as &[Effort]);
        assert!(!Thinking::None.requires_effort());
    }
}
