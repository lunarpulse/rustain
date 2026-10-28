//! Per-model Gemini CLI wire profile, adapted from oh-my-pi (catalog/src/models.json,
//! catalog/src/compat/rules/classes/gemini.kdl, catalog/src/compat/rules/providers/google-gemini-cli.kdl,
//! and, for collapsed logical ids, catalog/src/compat/collapse.ts with the
//! `google-gemini-cli` families of catalog/src/compat/rules/taxonomy/_collapse.kdl).
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

use super::collapse::{Family as VariantFamily, Role};
use std::borrow::Cow;

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

pub(super) const ALL_EFFORTS: &[Effort] = &Effort::CANONICAL;
pub(super) const LOW_HIGH: &[Effort] = &[Effort::Low, Effort::High];
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
/// Collapsed `{rev}` and derived families carry ids built at resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffortRoutes {
    pub off: Option<Cow<'static, str>>,
    pub minimal: Option<Cow<'static, str>>,
    pub low: Option<Cow<'static, str>>,
    pub medium: Option<Cow<'static, str>>,
    pub high: Option<Cow<'static, str>>,
}

impl EffortRoutes {
    /// Route for `effort` (`None` = off).
    pub(crate) fn route(&self, effort: Option<Effort>) -> Option<&str> {
        match effort {
            None => self.off.as_deref(),
            Some(Effort::Minimal) => self.minimal.as_deref(),
            Some(Effort::Low) => self.low.as_deref(),
            Some(Effort::Medium) => self.medium.as_deref(),
            Some(Effort::High) => self.high.as_deref(),
        }
    }
}

/// Wire-relevant facts of one Gemini CLI model.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// omp `requestModelId`: the wire id when no route applies (`resolveWireModelId`,
    /// model-thinking.ts:147-149); only a served collapsed row has one.
    pub request_model_id: Option<Cow<'static, str>>,
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
    request_model_id: None,
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
            request_model_id: None,
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
                off: Some(Cow::Borrowed("gemini-2.5-flash")),
                minimal: None,
                low: None,
                medium: None,
                high: None,
            }),
            suppress_when_off: false,
            request_model_id: None,
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
            request_model_id: None,
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
            request_model_id: None,
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
            request_model_id: None,
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
            request_model_id: None,
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
            request_model_id: None,
            multimodal_function_response: true,
        },
    ),
];

/// Profile of `id`: a bundled row, else omp's class rules for the parsed id.
pub(crate) fn for_model(id: &str) -> GeminiModelProfile {
    if let Some((_, profile)) = BUNDLED.iter().find(|(bundled, _)| *bundled == id) {
        return profile.clone();
    }
    derive(id)
}

/// What the adapter serves under the requested id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Served<'a> {
    /// No served row has this id.
    Absent,
    /// A served row with this exact id that no collapse produced.
    Row,
    /// A served collapsed row: the default wire id its record names (omp
    /// `requestModelId`, or the bare id of a derived `X`/`X-thinking` pair) and
    /// whether it reasons.
    Collapsed { wire: &'a str, reasoning: bool },
}

/// Profile of a requested id (R3/R4). Bundled ids keep their bundled profile.
/// A served collapsed reviewed row takes its family's table facts with the routes
/// its live members allow; an unserved reviewed logical id, member or alias selects
/// its family from the static table (omp `resolveVariantSelector`); a served
/// derived pair `X` routes `off → X` with no mandatory effort; any other id —
/// including a raw served row named like a logical id — is that row (omp's exact
/// lookup wins) under the class rules.
pub(crate) fn resolve(id: &str, served: Served<'_>) -> GeminiModelProfile {
    if let Some((_, profile)) = BUNDLED.iter().find(|(bundled, _)| *bundled == id) {
        return profile.clone();
    }
    match (super::collapse::reviewed(id), served) {
        (Some((family, Role::Logical)), Served::Collapsed { wire, reasoning }) => {
            family_profile(&family, Some((wire, reasoning)))
        }
        (Some((family, _)), Served::Absent) => family_profile(&family, None),
        (_, Served::Collapsed { wire, .. }) if wire == id => derived_pair(id),
        _ => derive(id),
    }
}

/// A reviewed family: the class facts of its logical id with the table's thinking
/// surface, routes and suppression. Unserved, it takes the whole static table and
/// reasons. Served, its default wire id is the first live non-retired member
/// (CL:954-974), so every non-retired member ranked before it is absent — all are
/// when it is retired or unknown; routes to absent or retired members drop and the
/// request falls back to that id (CL:905-916), and a family whose live members do
/// not reason carries no thinking (CL:918-920,978-982).
fn family_profile(family: &VariantFamily, served: Option<(&str, bool)>) -> GeminiModelProfile {
    let mut routes = family.routes.clone();
    let (reasoning, request_model_id) = match served {
        None => (true, None),
        Some((wire, reasoning)) => {
            let retired = |id: &str| family.retired.contains(&id);
            let live_from = family
                .members
                .iter()
                .position(|member| &**member == wire)
                .filter(|_| !retired(wire))
                .unwrap_or(family.members.len());
            let absent = &family.members[..live_from];
            for route in [
                &mut routes.off,
                &mut routes.minimal,
                &mut routes.low,
                &mut routes.medium,
                &mut routes.high,
            ] {
                let gone = route.as_deref().is_some_and(|target| {
                    retired(target) || absent.iter().any(|member| &**member == target)
                });
                if gone {
                    *route = None;
                }
            }
            (reasoning, Some(Cow::Owned(wire.to_owned())))
        }
    };
    GeminiModelProfile {
        reasoning,
        thinking: if reasoning {
            family.thinking
        } else {
            Thinking::None
        },
        routes: reasoning.then_some(routes),
        suppress_when_off: family.suppress_when_off,
        request_model_id,
        ..derive(&family.id)
    }
}

/// A derived `X`/`X-thinking` family (CL:557-599): the class thinking surface with
/// `requiresEffort` stripped (budget when the class has none), `off → X`.
fn derived_pair(id: &str) -> GeminiModelProfile {
    let class = derive(id);
    let thinking = match class.thinking {
        Thinking::Level { efforts, .. } => Thinking::Level {
            efforts,
            requires_effort: false,
        },
        Thinking::Budget { .. } | Thinking::None => Thinking::Budget {
            requires_effort: false,
        },
    };
    GeminiModelProfile {
        reasoning: true,
        thinking,
        routes: Some(EffortRoutes {
            off: Some(Cow::Owned(id.to_owned())),
            minimal: None,
            low: None,
            medium: None,
            high: None,
        }),
        suppress_when_off: false,
        ..class
    }
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
        request_model_id: None,
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
                off: Some(Cow::Borrowed("gemini-2.5-flash")),
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
                request_model_id: None,
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

    fn routes(profile: &GeminiModelProfile) -> [Option<&str>; 5] {
        let routes = profile.routes.as_ref().expect("routed profile");
        [
            routes.off.as_deref(),
            routes.minimal.as_deref(),
            routes.low.as_deref(),
            routes.medium.as_deref(),
            routes.high.as_deref(),
        ]
    }

    #[test]
    fn bundled_ids_resolve_to_their_bundled_profile() {
        for (id, _) in &BUNDLED {
            for served in [
                Served::Absent,
                Served::Row,
                Served::Collapsed {
                    wire: id,
                    reasoning: true,
                },
            ] {
                assert_eq!(resolve(id, served), for_model(id), "{id}");
            }
        }
    }

    #[test]
    fn reviewed_logical_ids_take_the_static_table() {
        let pro = resolve("gemini-3.1-pro", Served::Absent);
        assert_eq!(
            routes(&pro),
            [
                Some("gemini-3.1-pro-low"),
                None,
                Some("gemini-3.1-pro-low"),
                None,
                Some("gemini-pro-agent")
            ]
        );
        assert!(pro.reasoning && pro.suppress_when_off);
        assert_eq!(
            pro.thinking,
            Thinking::Level {
                efforts: LOW_HIGH,
                requires_effort: false
            }
        );
        // The class facts of the logical id stay.
        assert!(pro.first_call_sentinel && pro.multimodal_function_response && !pro.flash_leak);
        // A raw served row named like the logical id is that row (omp exact lookup).
        assert_eq!(
            resolve("gemini-3.1-pro", Served::Row),
            derive("gemini-3.1-pro")
        );
        assert_eq!(resolve("gemini-9-flash", Served::Row).routes, None);

        let flash = resolve("gemini-3.7-flash", Served::Absent);
        assert_eq!(
            routes(&flash),
            [
                None,
                Some("gemini-3.7-flash-low"),
                Some("gemini-3.7-flash-low"),
                Some("gemini-3.7-flash-medium"),
                Some("gemini-3.7-flash-high")
            ]
        );
        assert!(flash.thinking.requires_effort() && !flash.suppress_when_off && flash.flash_leak);

        let three = resolve("gemini-3-pro", Served::Absent);
        assert_eq!(routes(&three)[0], Some("gemini-3-pro-low"));
        assert!(three.suppress_when_off);
        let five = resolve("gemini-3.5-flash", Served::Absent);
        assert_eq!(routes(&five)[0], Some("gemini-3.5-flash-extra-low"));
        assert_eq!(routes(&five)[1], Some("gemini-3-flash-agent"));
    }

    #[test]
    fn unserved_members_and_aliases_select_their_family_but_a_served_row_wins() {
        assert_eq!(
            resolve("gemini-3-flash", Served::Absent),
            resolve("gemini-3.5-flash", Served::Absent)
        );
        assert_eq!(
            resolve("gemini-3.1-pro-high", Served::Absent),
            resolve("gemini-3.1-pro", Served::Absent)
        );
        assert_eq!(
            resolve("gemini-3-flash", Served::Row),
            derive("gemini-3-flash")
        );
        assert_eq!(resolve("gemini-3.1-pro-low", Served::Row).routes, None);
    }

    #[test]
    fn derived_pairs_route_off_to_themselves_without_a_mandatory_effort() {
        let pair = resolve(
            "gemini-9-pro",
            Served::Collapsed {
                wire: "gemini-9-pro",
                reasoning: true,
            },
        );
        assert_eq!(
            routes(&pair),
            [Some("gemini-9-pro"), None, None, None, None]
        );
        assert_eq!(
            pair.thinking,
            Thinking::Level {
                efforts: LOW_HIGH,
                requires_effort: false
            }
        );
        assert!(pair.reasoning && !pair.suppress_when_off);
        // Without the served pair the class rules require an effort (control).
        let class = resolve("gemini-9-pro", Served::Absent);
        assert!(class.thinking.requires_effort() && class.routes.is_none());
        let budget = resolve(
            "gemini-2.0-flash-x",
            Served::Collapsed {
                wire: "gemini-2.0-flash-x",
                reasoning: true,
            },
        );
        assert_eq!(
            budget.thinking,
            Thinking::Budget {
                requires_effort: false
            }
        );
        assert!(budget.reasoning);
    }

    #[test]
    fn served_collapsed_rows_drop_routes_to_absent_members() {
        let served = |id, wire, reasoning| resolve(id, Served::Collapsed { wire, reasoning });
        // Every member live: the whole table, defaulting to the first member.
        let pro = served("gemini-3.1-pro", "gemini-3.1-pro-low", true);
        assert_eq!(
            routes(&pro),
            routes(&resolve("gemini-3.1-pro", Served::Absent))
        );
        assert_eq!(pro.request_model_id.as_deref(), Some("gemini-3.1-pro-low"));
        // `-low` absent: only the route to the live default member stays.
        let agent = served("gemini-3.1-pro", "gemini-pro-agent", true);
        assert_eq!(
            routes(&agent),
            [None, None, None, None, Some("gemini-pro-agent")]
        );
        assert!(agent.suppress_when_off);
        // Only the retired member live: no route, no thinking.
        let retired = served("gemini-3.1-pro", "gemini-3.1-pro-high", false);
        assert_eq!(
            (retired.routes, retired.reasoning, retired.thinking),
            (None, false, Thinking::None)
        );
        assert_eq!(
            retired.request_model_id.as_deref(),
            Some("gemini-3.1-pro-high")
        );
        // Template: members ranked before the default are absent.
        let high = served("gemini-3.7-flash", "gemini-3.7-flash-high", true);
        assert_eq!(
            routes(&high),
            [None, None, None, None, Some("gemini-3.7-flash-high")]
        );
        let tiered = served("gemini-3.7-flash", "gemini-3.7-flash-tiered", true);
        assert_eq!(routes(&tiered), [None; 5]);
        assert_eq!(
            tiered.request_model_id.as_deref(),
            Some("gemini-3.7-flash-tiered")
        );
    }
}
