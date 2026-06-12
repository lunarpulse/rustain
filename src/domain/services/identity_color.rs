use crate::domain::models::ProfileIdentityColor;

pub fn derive_identity_color(name: &str, explicit: Option<u8>) -> ProfileIdentityColor {
    if let Some(color) = explicit {
        if color <= 15 {
            return ProfileIdentityColor(color);
        }
    }

    match name {
        "base" => ProfileIdentityColor(8),
        "coding" => ProfileIdentityColor(6),
        "personal-assistant" => ProfileIdentityColor(5),
        _ => {
            let hash = name
                .bytes()
                .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
            let color = (hash % 14) as u8 + 1;
            ProfileIdentityColor(color)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_explicit_field_wins() {
        let color = derive_identity_color("anything", Some(7));
        assert_eq!(color.0, 7);
    }

    #[test]
    fn test_builtin_base_returns_8() {
        assert_eq!(derive_identity_color("base", None).0, 8);
    }

    #[test]
    fn test_builtin_coding_returns_6() {
        assert_eq!(derive_identity_color("coding", None).0, 6);
    }

    #[test]
    fn test_builtin_personal_assistant_returns_5() {
        assert_eq!(derive_identity_color("personal-assistant", None).0, 5);
    }

    #[test]
    fn test_hash_deterministic_across_invocations() {
        let name = "my-custom-profile";
        let first = derive_identity_color(name, None).0;
        for _ in 0..100 {
            assert_eq!(derive_identity_color(name, None).0, first);
        }
    }

    #[test]
    fn test_hash_in_range_1_to_14() {
        let mut seen = HashSet::new();
        for i in 0..1000u32 {
            let name = format!("profile_{i}");
            let color = derive_identity_color(&name, None).0;
            assert!(
                (1..=14).contains(&color),
                "color {} for '{}' out of range",
                color,
                name
            );
            seen.insert(color);
        }
        assert!(
            seen.len() >= 3,
            "hash should produce at least a few different colors"
        );
    }

    #[test]
    fn test_zero_and_15_never_returned_for_hash_fallback() {
        for i in 0..500u32 {
            let name = format!("test_profile_{}", i);
            let color = derive_identity_color(&name, None).0;
            assert_ne!(color, 0);
            assert_ne!(color, 15);
        }
    }
}
