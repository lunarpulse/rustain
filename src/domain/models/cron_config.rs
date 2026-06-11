//! Cron scheduler configuration loaded from `~/.config/rustain/cron.toml`.
//!
//! Pure domain model only: parser/runtime validation lives in the cron scheduler
//! adapter so the domain layer stays free of adapter/runtime crates.

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct CronConfig {
    #[serde(default)]
    pub jobs: Vec<CronJob>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct CronJob {
    pub name: String,
    pub schedule: String,
    pub prompt: String,
    #[serde(default)]
    pub forward: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_config_deserializes_jobs_and_forward_default() {
        let parsed: CronConfig = toml::from_str(
            r#"
            [[jobs]]
            name = "morning"
            schedule = "0 9 * * *"
            prompt = "Brief me"
            "#,
        )
        .unwrap();

        assert_eq!(parsed.jobs.len(), 1);
        assert_eq!(parsed.jobs[0].name, "morning");
        assert_eq!(parsed.jobs[0].schedule, "0 9 * * *");
        assert_eq!(parsed.jobs[0].prompt, "Brief me");
        assert!(!parsed.jobs[0].forward);
    }
}
