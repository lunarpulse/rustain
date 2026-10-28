//! Effort-variant collapse for the Cloud Code Assist (Gemini CLI) roster, adapted
//! from oh-my-pi (catalog/src/compat/collapse.ts `collapseWithTable`,
//! `deriveThinkingPairFamilies` and the `{rev}` template instantiation;
//! catalog/src/compat/taxonomy.ts `stripThinkingVariantSuffix`;
//! catalog/src/compat/revision.ts; the `google-gemini-cli` gemini families of
//! catalog/src/compat/rules/taxonomy/_collapse.kdl).
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

// Port scope: only what the `google-gemini-cli` table exercises. Its non-gemini
// families (`gpt-oss-120b`, Claude) are omitted because the gemini-class filter
// drops their ids on both discovery paths; Cursor tiers, max-mode, budgets and
// default-member reconciliation have no `google-gemini-cli` family and are not
// ported. Already-collapsed input exists only as the bundled `gemini-2.5-flash`
// reference on the quota path; it passes through with its raw members removed.

use super::profile::{ALL_EFFORTS, EffortRoutes, LOW_HIGH, Thinking};
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

/// One roster row with the fields the catalogue keeps (omp `ModelSpec`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Spec {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    /// `input` includes `image`.
    pub image: bool,
    pub context: u32,
    /// Already a collapsed family (omp: carries `effortRouting`).
    pub collapsed: bool,
    /// A collapsed row's default wire id: omp `requestModelId` (the first live
    /// non-retired member, CL:954-974), or the bare id of a derived `X`/`X-thinking`
    /// pair. Its record carries it, so the served row resolves without the roster.
    pub wire: Option<String>,
}

/// One collapsed family (omp `EffortVariantFamily`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Family {
    pub id: Cow<'static, str>,
    pub name: Cow<'static, str>,
    /// Wire ids in priority order.
    pub members: Vec<Cow<'static, str>>,
    /// Members upstream no longer serves: consumed, never routed to.
    pub retired: &'static [&'static str],
    /// Retired selector ids that name this family without being members.
    pub aliases: &'static [&'static str],
    pub routes: EffortRoutes,
    pub thinking: Thinking,
    pub suppress_when_off: bool,
    pub preserve_absent_effort_routes: bool,
}

/// How a selector names a reviewed family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Logical,
    Member,
    Alias,
}

/// `_collapse.kdl` `variant-family` row; routes are off, minimal, low, medium, high.
struct Row {
    id: &'static str,
    name: &'static str,
    members: &'static [&'static str],
    retired: &'static [&'static str],
    aliases: &'static [&'static str],
    routes: [Option<&'static str>; 5],
    thinking: Thinking,
    suppress_when_off: bool,
    preserve_absent_effort_routes: bool,
}

/// omp `REVISION_PLACEHOLDER`.
const PLACEHOLDER: &str = "{rev}";
/// `revision=">=3.6"` of the template.
const TEMPLATE_MIN_REVISION: (u32, u32, u32) = (3, 6, 0);

/// `_collapse.kdl:165-174`.
const TEMPLATE: Row = Row {
    id: "gemini-{rev}-flash",
    name: "Gemini {rev} Flash",
    members: &[
        "gemini-{rev}-flash-low",
        "gemini-{rev}-flash-medium",
        "gemini-{rev}-flash-high",
        "gemini-{rev}-flash-tiered",
    ],
    retired: &[],
    aliases: &[],
    routes: [
        None,
        Some("gemini-{rev}-flash-low"),
        Some("gemini-{rev}-flash-low"),
        Some("gemini-{rev}-flash-medium"),
        Some("gemini-{rev}-flash-high"),
    ],
    thinking: Thinking::Level {
        efforts: ALL_EFFORTS,
        requires_effort: true,
    },
    suppress_when_off: false,
    preserve_absent_effort_routes: false,
};

/// The concrete gemini families, in `_collapse.kdl` order (`:175-207,245-255`).
const FAMILIES: [Row; 4] = [
    Row {
        id: "gemini-3.5-flash",
        name: "Gemini 3.5 Flash",
        members: &[
            "gemini-3.5-flash-extra-low",
            "gemini-3.5-flash-low",
            "gemini-3-flash-agent",
        ],
        retired: &[],
        aliases: &["gemini-3-flash"],
        routes: [
            Some("gemini-3.5-flash-extra-low"),
            Some("gemini-3-flash-agent"),
            Some("gemini-3.5-flash-extra-low"),
            Some("gemini-3.5-flash-extra-low"),
            Some("gemini-3.5-flash-low"),
        ],
        thinking: Thinking::Level {
            efforts: ALL_EFFORTS,
            requires_effort: false,
        },
        suppress_when_off: true,
        preserve_absent_effort_routes: false,
    },
    Row {
        id: "gemini-3.1-pro",
        name: "Gemini 3.1 Pro",
        members: &[
            "gemini-3.1-pro-low",
            "gemini-pro-agent",
            "gemini-3.1-pro-high",
        ],
        retired: &["gemini-3.1-pro-high"],
        aliases: &[],
        routes: [
            Some("gemini-3.1-pro-low"),
            None,
            Some("gemini-3.1-pro-low"),
            None,
            Some("gemini-pro-agent"),
        ],
        thinking: Thinking::Level {
            efforts: LOW_HIGH,
            requires_effort: false,
        },
        suppress_when_off: true,
        preserve_absent_effort_routes: false,
    },
    Row {
        id: "gemini-3-pro",
        name: "Gemini 3 Pro",
        members: &["gemini-3-pro-low", "gemini-3-pro-high"],
        retired: &[],
        aliases: &[],
        routes: [
            Some("gemini-3-pro-low"),
            None,
            Some("gemini-3-pro-low"),
            None,
            Some("gemini-3-pro-high"),
        ],
        thinking: Thinking::Level {
            efforts: LOW_HIGH,
            requires_effort: false,
        },
        suppress_when_off: true,
        preserve_absent_effort_routes: false,
    },
    Row {
        id: "gemini-2.5-flash",
        name: "Gemini 2.5 Flash",
        members: &["gemini-2.5-flash", "gemini-2.5-flash-thinking"],
        retired: &[],
        aliases: &[],
        routes: [
            Some("gemini-2.5-flash"),
            Some("gemini-2.5-flash-thinking"),
            Some("gemini-2.5-flash-thinking"),
            Some("gemini-2.5-flash-thinking"),
            Some("gemini-2.5-flash-thinking"),
        ],
        thinking: Thinking::Budget {
            requires_effort: false,
        },
        suppress_when_off: false,
        preserve_absent_effort_routes: true,
    },
];

/// `_collapse.kdl` `pair-token`.
const PAIR_TOKENS: [&str; 3] = ["thinking", "reasoning", "reasoner"];
/// `_collapse.kdl` `thinking-suffix`.
const THINKING_SUFFIX: &str = "-thinking";

impl Row {
    /// The family with `{rev}` filled in, or the concrete row as is.
    fn family(&self, rev: Option<&str>) -> Family {
        let fill = |text: &'static str| -> Cow<'static, str> {
            match rev {
                Some(rev) => Cow::Owned(text.replace(PLACEHOLDER, rev)),
                None => Cow::Borrowed(text),
            }
        };
        let [off, minimal, low, medium, high] = self.routes.map(|route| route.map(&fill));
        Family {
            id: fill(self.id),
            name: fill(self.name),
            members: self.members.iter().copied().map(&fill).collect(),
            retired: self.retired,
            aliases: self.aliases,
            routes: EffortRoutes {
                off,
                minimal,
                low,
                medium,
                high,
            },
            thinking: self.thinking,
            suppress_when_off: self.suppress_when_off,
            preserve_absent_effort_routes: self.preserve_absent_effort_routes,
        }
    }
}

impl Family {
    /// Routing targets in omp `VARIANT_ROUTING_KEYS` order: off, then efforts.
    fn targets(&self) -> [Option<&str>; 5] {
        let routes = &self.routes;
        [
            routes.off.as_deref(),
            routes.minimal.as_deref(),
            routes.low.as_deref(),
            routes.medium.as_deref(),
            routes.high.as_deref(),
        ]
    }
}

/// omp `parseRevision`: one to three `.`/`-` separated components, each ≤ 255.
fn parse_revision(value: &str) -> Option<(u32, u32, u32)> {
    let mut out = [0_u32; 3];
    for (count, part) in value.split(['.', '-']).enumerate() {
        if count == 3 || part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out[count] = part.parse().ok().filter(|component| *component <= 255)?;
    }
    Some((out[0], out[1], out[2]))
}

/// The template revision `id` carries under `pattern` (one `{rev}`), matched
/// case-insensitively as omp's `(\d+(?:\.\d+){0,2})` alternative.
fn template_revision<'a>(pattern: &str, id: &'a str) -> Option<&'a str> {
    let (prefix, suffix) = pattern.split_once(PLACEHOLDER)?;
    if id.len() < prefix.len() + suffix.len()
        || !id.is_char_boundary(prefix.len())
        || !id.is_char_boundary(id.len() - suffix.len())
        || !id[..prefix.len()].eq_ignore_ascii_case(prefix)
        || !id[id.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    {
        return None;
    }
    let rev = &id[prefix.len()..id.len() - suffix.len()];
    let groups: Vec<&str> = rev.split('.').collect();
    (groups.len() <= 3
        && groups
            .iter()
            .all(|group| !group.is_empty() && group.bytes().all(|b| b.is_ascii_digit())))
    .then_some(rev)
}

/// omp `templateFamilyFor`: the instantiated template family naming `id` as its
/// logical id or a member, when the revision is in range.
fn template_family(id: &str) -> Option<(Family, Role)> {
    let alternatives = std::iter::once((TEMPLATE.id, Role::Logical)).chain(
        TEMPLATE
            .members
            .iter()
            .map(|member| (*member, Role::Member)),
    );
    for (pattern, role) in alternatives {
        let Some(rev) = template_revision(pattern, id) else {
            continue;
        };
        return parse_revision(rev)
            .filter(|revision| *revision >= TEMPLATE_MIN_REVISION)
            .map(|_| (TEMPLATE.family(Some(rev)), role));
    }
    None
}

/// The reviewed family that names `id` as its logical id, a member or an alias
/// (omp's reviewed alias index, case-insensitive): concrete rows first, then the
/// `{rev}` template.
pub(crate) fn reviewed(id: &str) -> Option<(Family, Role)> {
    for row in &FAMILIES {
        let role = if row.id.eq_ignore_ascii_case(id) {
            Role::Logical
        } else if row.members.iter().any(|m| m.eq_ignore_ascii_case(id)) {
            Role::Member
        } else if row.aliases.iter().any(|a| a.eq_ignore_ascii_case(id)) {
            Role::Alias
        } else {
            continue;
        };
        return Some((row.family(None), role));
    }
    template_family(id)
}

/// The reviewed families one pass applies: every concrete row, then one template
/// instance per revision present in `specs` (omp `instantiateTemplates`).
fn reviewed_families(specs: &[Spec]) -> Vec<Family> {
    let mut families: Vec<Family> = FAMILIES.iter().map(|row| row.family(None)).collect();
    let mut seen: HashSet<String> = families.iter().map(|f| f.id.to_string()).collect();
    for spec in specs {
        if let Some((family, _)) = template_family(&spec.id)
            && seen.insert(family.id.to_string())
        {
            families.push(family);
        }
    }
    families
}

/// omp `collapseWithTable` restricted to the shapes above: each family with live
/// members becomes one row named by the family at its first member's position;
/// members are removed; an already-collapsed row keeps itself.
fn collapse_with(specs: &[Spec], families: &[Family]) -> Vec<Spec> {
    let mut by_id: HashMap<&str, &Spec> = HashMap::new();
    for spec in specs {
        by_id.entry(spec.id.as_str()).or_insert(spec);
    }
    let mut replacement: HashMap<String, Spec> = HashMap::new();
    let mut family_of: HashMap<String, String> = HashMap::new();
    for family in families {
        let existing = by_id.get(family.id.as_ref()).copied();
        let existing_collapsed = existing.is_some_and(|spec| spec.collapsed);
        let present: Vec<&str> = family
            .members
            .iter()
            .map(AsRef::as_ref)
            .filter(|id| by_id.contains_key(id) && !(*id == family.id && existing_collapsed))
            .collect();
        if present.is_empty() {
            continue;
        }
        for id in &present {
            family_of.insert((*id).to_owned(), family.id.to_string());
        }
        if existing.is_some() {
            family_of.insert(family.id.to_string(), family.id.to_string());
        }
        if let Some(existing) = existing.filter(|_| existing_collapsed) {
            replacement.insert(family.id.to_string(), existing.clone());
            continue;
        }
        let members: Vec<&Spec> = present.iter().map(|id| by_id[id]).collect();
        let live: HashSet<&str> = present.iter().copied().collect();
        // A family routing an effort to a live (or preserved) member reasons.
        let effort_route = family.targets()[1..].iter().flatten().any(|target| {
            (live.contains(target) || family.preserve_absent_effort_routes)
                && !family.retired.contains(target)
        });
        // omp `requestModelId`: the first live non-retired member, else the first live.
        let wire = present
            .iter()
            .copied()
            .find(|id| !family.retired.contains(id))
            .unwrap_or(present[0]);
        replacement.insert(
            family.id.to_string(),
            Spec {
                id: family.id.to_string(),
                name: family.name.to_string(),
                reasoning: members.iter().any(|spec| spec.reasoning) || effort_route,
                image: members.iter().any(|spec| spec.image),
                context: members.iter().map(|spec| spec.context).max().unwrap_or(0),
                collapsed: true,
                wire: Some(wire.to_owned()),
            },
        );
    }
    if replacement.is_empty() {
        return specs.to_vec();
    }
    let mut emitted = HashSet::new();
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        match family_of.get(&spec.id) {
            None => out.push(spec.clone()),
            Some(family) => {
                if emitted.insert(family.as_str())
                    && let Some(collapsed) = replacement.get(family)
                {
                    out.push(collapsed.clone());
                }
            }
        }
    }
    out
}

/// omp `stripThinkingVariantSuffix`: removes the first `-thinking`/`-reasoning`/
/// `-reasoner` token not followed by a token character nor negated (`non-`/`no-`).
fn strip_pair_token(id: &str) -> Option<String> {
    let lower = id.to_ascii_lowercase();
    for token in PAIR_TOKENS {
        let needle = format!("-{token}");
        let mut from = 0;
        while let Some(found) = lower[from..].find(&needle) {
            let index = from + found;
            let end = index + needle.len();
            let continued = lower
                .as_bytes()
                .get(end)
                .is_some_and(|b| b.is_ascii_digit() || b.is_ascii_lowercase());
            let word_start = lower[..index]
                .rfind(|c: char| !(c.is_ascii_digit() || c.is_ascii_lowercase()))
                .map_or(0, |at| at + 1);
            let preceding = &lower[word_start..index];
            if !continued && preceding != "non" && preceding != "no" {
                let stripped = format!("{}{}", &id[..index], &id[end..]);
                return (!stripped.is_empty()).then_some(stripped);
            }
            from = index + 1;
        }
    }
    None
}

/// The bare id an `X-thinking`-style sibling pairs with: the `-thinking` suffix
/// (omp `collapseVariantId`), else the first pair token.
fn pair_base(id: &str) -> Option<String> {
    let cut = id.len().checked_sub(THINKING_SUFFIX.len())?;
    if id.is_char_boundary(cut) && id[cut..].eq_ignore_ascii_case(THINKING_SUFFIX) {
        return Some(id[..cut].to_owned());
    }
    strip_pair_token(id)
}

/// omp `deriveThinkingPairFamilies` against the reviewed table: for each live
/// `X` + `X-thinking` pair neither of which the table claims, a family `X` with
/// `off → X` and every effort → the thinking id. Quota rows carry no thinking
/// surface or price, so the class surface applies and every pair qualifies.
fn derive_pairs(specs: &[Spec]) -> Vec<Family> {
    let mut by_id: HashMap<&str, &Spec> = HashMap::new();
    for spec in specs {
        by_id.entry(spec.id.as_str()).or_insert(spec);
    }
    let mut families = Vec::new();
    for spec in specs {
        let Some(base_id) = pair_base(&spec.id).filter(|base| *base != spec.id) else {
            continue;
        };
        let Some(base) = by_id.get(base_id.as_str()).copied() else {
            continue;
        };
        if reviewed(&spec.id).is_some() || reviewed(&base_id).is_some() {
            continue;
        }
        let thinking: Cow<'static, str> = Cow::Owned(spec.id.clone());
        families.push(Family {
            id: Cow::Owned(base.id.clone()),
            name: Cow::Owned(base.name.clone()),
            members: vec![Cow::Owned(base.id.clone()), thinking.clone()],
            retired: &[],
            aliases: &[],
            routes: EffortRoutes {
                off: Some(Cow::Owned(base.id.clone())),
                minimal: Some(thinking.clone()),
                low: Some(thinking.clone()),
                medium: Some(thinking.clone()),
                high: Some(thinking),
            },
            thinking: Thinking::Budget {
                requires_effort: false,
            },
            suppress_when_off: false,
            preserve_absent_effort_routes: false,
        });
    }
    families
}

/// Antigravity roster: the reviewed table only (AG:229-232 with PM:164-172).
pub(super) fn reviewed_only(specs: &[Spec]) -> Vec<Spec> {
    collapse_with(specs, &reviewed_families(specs))
}

/// Quota roster: `collapseVariants` without a table argument — the reviewed table,
/// then the derived `X`/`X-thinking` pairs (CL:1104-1116).
pub(super) fn reviewed_and_pairs(specs: &[Spec]) -> Vec<Spec> {
    let reviewed = collapse_with(specs, &reviewed_families(specs));
    let pairs = derive_pairs(&reviewed);
    if pairs.is_empty() {
        reviewed
    } else {
        collapse_with(&reviewed, &pairs)
    }
}

/// `a.name.localeCompare(b.name) || a.id.localeCompare(b.id)` (AG:233, D:164).
pub(super) fn sort(specs: &mut [Spec]) {
    specs.sort_by(|a, b| {
        locale_compare(&a.name, &b.name).then_with(|| locale_compare(&a.id, &b.id))
    });
}

/// CLDR root order of the printable ASCII punctuation and symbols (whitespace
/// first, then these, then digits, then letters).
const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

/// Primary weight of `c` under the CLDR root collation for ASCII; control
/// characters other than whitespace are ignorable (`None`). Non-ASCII sorts after
/// every ASCII letter by code point (an approximation outside ASCII).
fn primary(c: char) -> Option<u32> {
    match c {
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' => Some(c as u32),
        ' ' => Some(0x20),
        '0'..='9' => Some(0x100 + (c as u32 - '0' as u32)),
        'a'..='z' => Some(0x200 + (c as u32 - 'a' as u32)),
        'A'..='Z' => Some(0x200 + (c as u32 - 'A' as u32)),
        c if c.is_ascii_control() => None,
        c if c.is_ascii() => PUNCTUATION.find(c).map(|at| 0x40 + at as u32),
        c => Some(0x1000 + c as u32),
    }
}

/// `String.prototype.localeCompare` with the default (root) locale for the ids
/// and names discovery sees: primary level (case-insensitive, punctuation <
/// digits < letters), then tertiary (lowercase before uppercase).
pub(super) fn locale_compare(a: &str, b: &str) -> Ordering {
    let primaries = |s: &str| s.chars().filter_map(primary).collect::<Vec<_>>();
    primaries(a).cmp(&primaries(b)).then_with(|| {
        let tertiary = |s: &str| {
            s.chars()
                .filter(|c| primary(*c).is_some())
                .map(|c| c.is_ascii_uppercase())
                .collect::<Vec<_>>()
        };
        tertiary(a).cmp(&tertiary(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, name: &str, reasoning: bool, context: u32) -> Spec {
        Spec {
            id: id.into(),
            name: name.into(),
            reasoning,
            image: true,
            context,
            collapsed: false,
            wire: None,
        }
    }

    fn ids(specs: &[Spec]) -> Vec<&str> {
        specs.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn template_boundaries_and_instantiation() {
        // 3.5 belongs to the concrete family, never the template (rev < 3.6).
        assert_eq!(
            reviewed("gemini-3.5-flash-low").map(|(f, r)| (f.id, r)),
            Some((Cow::Borrowed("gemini-3.5-flash"), Role::Member))
        );
        assert!(template_family("gemini-3.5-flash-low").is_none());
        assert!(template_family("gemini-3.5-flash").is_none());
        // 3.6 and later instantiate, including the `-tiered` member.
        let (family, role) = reviewed("gemini-3.6-flash-tiered").unwrap();
        assert_eq!(role, Role::Member);
        assert_eq!(family.id, "gemini-3.6-flash");
        assert_eq!(family.name, "Gemini 3.6 Flash");
        assert_eq!(
            family.members,
            [
                "gemini-3.6-flash-low",
                "gemini-3.6-flash-medium",
                "gemini-3.6-flash-high",
                "gemini-3.6-flash-tiered"
            ]
        );
        assert_eq!(family.routes.off, None);
        assert_eq!(
            family.routes.minimal.as_deref(),
            Some("gemini-3.6-flash-low")
        );
        assert_eq!(family.routes.low.as_deref(), Some("gemini-3.6-flash-low"));
        assert_eq!(
            family.routes.medium.as_deref(),
            Some("gemini-3.6-flash-medium")
        );
        assert_eq!(family.routes.high.as_deref(), Some("gemini-3.6-flash-high"));
        assert!(family.thinking.requires_effort() && !family.suppress_when_off);
        assert_eq!(
            reviewed("gemini-10.0.1-flash").unwrap().0.id,
            "gemini-10.0.1-flash"
        );
        assert_eq!(
            reviewed("GEMINI-3.7-FLASH-LOW").unwrap().0.id,
            "gemini-3.7-flash"
        );
        for outside in [
            "gemini-3.6.1.2-flash",
            "gemini-256-flash",
            "gemini-3.7-flash-lite",
            "gemini-3.7-flash-agent",
            "gemini-.7-flash",
            "gemini-3.7-pro",
        ] {
            assert!(reviewed(outside).is_none(), "{outside}");
        }
    }

    #[test]
    fn alias_and_retired_member_name_their_family() {
        let (family, role) = reviewed("gemini-3-flash").unwrap();
        assert_eq!(
            (family.id.as_ref(), role),
            ("gemini-3.5-flash", Role::Alias)
        );
        let (family, role) = reviewed("gemini-3.1-pro-high").unwrap();
        assert_eq!((family.id.as_ref(), role), ("gemini-3.1-pro", Role::Member));
        assert_eq!(family.retired, ["gemini-3.1-pro-high"]);
        assert_eq!(family.routes.high.as_deref(), Some("gemini-pro-agent"));
    }

    #[test]
    fn members_collapse_into_the_family_with_the_largest_context() {
        let specs = [
            spec("gemini-3.1-pro-high", "High", false, 900),
            spec("gemini-x", "X", false, 10),
            spec("gemini-3.1-pro-low", "Low", true, 300),
            spec("gemini-pro-agent", "Agent", false, 700),
        ];
        let out = reviewed_only(&specs);
        assert_eq!(ids(&out), ["gemini-3.1-pro", "gemini-x"]);
        assert_eq!(out[0].name, "Gemini 3.1 Pro");
        assert_eq!(out[0].context, 900);
        assert!(out[0].reasoning);
        // The default wire id is the first live non-retired member, in member order.
        assert_eq!(out[0].wire.as_deref(), Some("gemini-3.1-pro-low"));
    }

    #[test]
    fn a_retired_only_family_still_collapses_and_an_effort_route_implies_reasoning() {
        let out = reviewed_only(&[spec("gemini-3.1-pro-high", "High", false, 5)]);
        assert_eq!(ids(&out), ["gemini-3.1-pro"]);
        // The only effort target present is retired: no reasoning from routing.
        assert!(!out[0].reasoning);
        // Only a retired member is live: it is the default (omp `rawPresent[0]`).
        assert_eq!(out[0].wire.as_deref(), Some("gemini-3.1-pro-high"));
        let out = reviewed_only(&[spec("gemini-pro-agent", "Agent", false, 5)]);
        assert!(out[0].reasoning, "high → gemini-pro-agent is live");
        assert_eq!(out[0].wire.as_deref(), Some("gemini-pro-agent"));
        // A raw logical-id row with no live member is no family (omp: inert).
        let raw = [spec("gemini-9-flash", "gemini-9-flash", true, 5)];
        assert_eq!(reviewed_only(&raw), raw);
    }

    #[test]
    fn collapsed_bundled_reference_keeps_itself_and_drops_its_raw_member() {
        let mut bundled = spec("gemini-2.5-flash", "Gemini 2.5 Flash", true, 1_048_576);
        bundled.collapsed = true;
        let out = reviewed_and_pairs(&[
            bundled.clone(),
            spec(
                "gemini-2.5-flash-thinking",
                "gemini-2.5-flash-thinking",
                true,
                7,
            ),
        ]);
        assert_eq!(out, [bundled]);
    }

    #[test]
    fn derived_pairs_form_only_for_unclaimed_twins() {
        let specs = [
            spec(
                "gemini-9-pro-thinking",
                "gemini-9-pro-thinking",
                true,
                2_000_000,
            ),
            spec("gemini-9-pro", "gemini-9-pro", true, 1_048_576),
            // The `{rev}` template claims gemini-9-flash: no pair.
            spec("gemini-9-flash", "gemini-9-flash", true, 1),
            spec(
                "gemini-9-flash-thinking",
                "gemini-9-flash-thinking",
                true,
                1,
            ),
            // A thinking id without its base stays.
            spec("gemini-8-pro-thinking", "gemini-8-pro-thinking", true, 1),
        ];
        let out = reviewed_and_pairs(&specs);
        assert_eq!(
            ids(&out),
            [
                "gemini-9-pro",
                "gemini-9-flash",
                "gemini-9-flash-thinking",
                "gemini-8-pro-thinking"
            ]
        );
        assert!(out[0].reasoning);
        assert_eq!(out[0].wire.as_deref(), Some("gemini-9-pro"));
        assert_eq!(out[0].name, "gemini-9-pro");
        assert_eq!(out[0].context, 2_000_000);
        // The Antigravity path never derives pairs.
        assert_eq!(ids(&reviewed_only(&specs)), ids(&specs));
    }

    #[test]
    fn pair_tokens_follow_omp() {
        assert_eq!(
            pair_base("gemini-9-pro-thinking").as_deref(),
            Some("gemini-9-pro")
        );
        assert_eq!(pair_base("gemini-9-Thinking").as_deref(), Some("gemini-9"));
        assert_eq!(
            pair_base("gemini-9-reasoning-pro").as_deref(),
            Some("gemini-9-pro")
        );
        assert_eq!(pair_base("gemini-9-reasoner").as_deref(), Some("gemini-9"));
        // `thinking-suffix` collapse has no negation exception; pair tokens do.
        assert_eq!(
            pair_base("gemini-9-non-thinking").as_deref(),
            Some("gemini-9-non")
        );
        assert_eq!(strip_pair_token("gemini-9-non-thinking"), None);
        assert_eq!(pair_base("gemini-9-no-reasoning"), None);
        assert_eq!(pair_base("gemini-9-thinkingx"), None);
        assert_eq!(pair_base("gemini-9-thinking2"), None);
        assert_eq!(pair_base("-thinking"), Some(String::new()));
        assert_eq!(strip_pair_token("-thinking"), None);
    }

    #[test]
    fn revisions_parse_like_omp() {
        assert_eq!(parse_revision("3.6"), Some((3, 6, 0)));
        assert_eq!(parse_revision("10.0.1"), Some((10, 0, 1)));
        assert_eq!(parse_revision("256"), None);
        assert_eq!(parse_revision("1.2.3.4"), None);
        assert_eq!(parse_revision(""), None);
    }

    #[test]
    fn locale_order_is_case_insensitive_first_then_lowercase_first() {
        let mut names = vec![
            "Gemini Z",
            "gemini a",
            "Gemini 3 Pro",
            "gemini-2.6-flash",
            "a",
            "A",
            "_x",
            "0",
        ];
        names.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(
            names,
            [
                "_x",
                "0",
                "a",
                "A",
                "Gemini 3 Pro",
                "gemini a",
                "Gemini Z",
                "gemini-2.6-flash"
            ]
        );
        assert_eq!(locale_compare("gemini-3", "gemini.3"), Ordering::Less);
        assert_eq!(locale_compare("ab", "a"), Ordering::Greater);
    }
}
