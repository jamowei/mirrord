use mirrord_config_derive::MirrordConfig;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::source::MirrordConfigSource;

/// Configuration for the mirrord-agent pod that is spawned in the Kubernetes cluster.
#[derive(MirrordConfig, Deserialize, Default, PartialEq, Eq, Clone, Debug, JsonSchema)]
#[serde(deny_unknown_fields)]
#[config(map_to = AgentConfig)]
pub struct AgentFileConfig {
    /// Log level for the agent.
    ///
    /// Supports anything that would work with `RUST_LOG`.
    #[config(env = "MIRRORD_AGENT_RUST_LOG", default = "info")]
    pub log_level: Option<String>,

    /// Namespace where the agent shall live.
    ///
    /// Defaults to the current kubernetes namespace.
    #[config(env = "MIRRORD_AGENT_NAMESPACE")]
    pub namespace: Option<String>,

    /// Name of the agent's docker image.
    ///
    /// Useful when a custom build of mirrord-agent is required, or when using an internal
    /// registry.
    ///
    /// Defaults to the latest stable image.
    #[config(env = "MIRRORD_AGENT_IMAGE")]
    pub image: Option<String>,

    /// Controls when a new agent image is downloaded.
    ///
    /// Supports any valid kubernetes [image pull
    /// policy](https://kubernetes.io/docs/concepts/containers/images/#image-pull-policy)
    #[config(env = "MIRRORD_AGENT_IMAGE_PULL_POLICY", default = "IfNotPresent")]
    pub image_pull_policy: Option<String>,

    /// Controls how long the agent pod persists for, after the local process terminated (in
    /// seconds).
    ///
    /// Can be useful for collecting logs.
    #[config(env = "MIRRORD_AGENT_TTL", default = "0")]
    pub ttl: Option<u16>,

    /// Runs the agent as an [ephemeral
    /// container](https://kubernetes.io/docs/concepts/workloads/pods/ephemeral-containers/)
    #[config(env = "MIRRORD_EPHEMERAL_CONTAINER", default = "false")]
    pub ephemeral: Option<bool>,

    /// Controls how long the agent lives when there are no connections.
    ///
    /// Each connection has its own heartbeat mechanism, so even if the local application has no
    /// messages, the agent stays alive until there are no more heartbeat messages.
    #[config(env = "MIRRORD_AGENT_COMMUNICATION_TIMEOUT")]
    pub communication_timeout: Option<u16>,

    /// Controls how long to wait for the agent to finish initialization.
    ///
    /// If initialization takes longer than this value, mirrord exits.
    #[config(env = "MIRRORD_AGENT_STARTUP_TIMEOUT", default = "60")]
    pub startup_timeout: Option<u64>,
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::{config::MirrordConfig, util::testing::with_env_vars};

    #[rstest]
    fn default(
        #[values((None, "info"), (Some("trace"), "trace"))] log_level: (Option<&str>, &str),
        #[values((None, None), (Some("app"), Some("app")))] namespace: (Option<&str>, Option<&str>),
        #[values((None, None), (Some("test"), Some("test")))] image: (Option<&str>, Option<&str>),
        #[values((None, "IfNotPresent"), (Some("Always"), "Always"))] image_pull_policy: (
            Option<&str>,
            &str,
        ),
        #[values((None, 0), (Some("30"), 30))] ttl: (Option<&str>, u16),
        #[values((None, false), (Some("true"), true))] ephemeral: (Option<&str>, bool),
        #[values((None, None), (Some("30"), Some(30)))] communication_timeout: (
            Option<&str>,
            Option<u16>,
        ),
        #[values((None, 60), (Some("30"), 30))] startup_timeout: (Option<&str>, u64),
    ) {
        with_env_vars(
            vec![
                ("MIRRORD_AGENT_RUST_LOG", log_level.0),
                ("MIRRORD_AGENT_NAMESPACE", namespace.0),
                ("MIRRORD_AGENT_IMAGE", image.0),
                ("MIRRORD_AGENT_IMAGE_PULL_POLICY", image_pull_policy.0),
                ("MIRRORD_AGENT_TTL", ttl.0),
                ("MIRRORD_EPHEMERAL_CONTAINER", ephemeral.0),
                (
                    "MIRRORD_AGENT_COMMUNICATION_TIMEOUT",
                    communication_timeout.0,
                ),
                ("MIRRORD_AGENT_STARTUP_TIMEOUT", startup_timeout.0),
            ],
            || {
                let agent = AgentFileConfig::default().generate_config().unwrap();

                assert_eq!(agent.log_level, log_level.1);
                assert_eq!(agent.namespace.as_deref(), namespace.1);
                assert_eq!(agent.image.as_deref(), image.1);
                assert_eq!(agent.image_pull_policy, image_pull_policy.1);
                assert_eq!(agent.ttl, ttl.1);
                assert_eq!(agent.ephemeral, ephemeral.1);
                assert_eq!(agent.communication_timeout, communication_timeout.1);
                assert_eq!(agent.startup_timeout, startup_timeout.1);
            },
        );
    }
}
