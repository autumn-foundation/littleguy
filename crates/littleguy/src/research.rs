//! The research tool: one `sweep` call reads many sources (ADR 0044).
//!
//! autumn-harvest runs the call as an `agent_sweep` child of the assistant
//! run. Each source is one read-only run of the [`RESEARCH_KIND`] kind. Its
//! transcript stays out of the assistant window. Only one short row per
//! source comes back, with the spend of the call.
//!
//! The owner sets every bound in `[research]`, and no bound has a default
//! of its own (ADR 0034). The engine needs a ceiling on each call, so a
//! call with no ceiling in the config gets the largest one it takes.

use std::sync::Arc;

use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::sampling::{
    PolicyAudit, PolicyChange, PolicyRead, PolicyStoreError, SamplingPolicy, SamplingPolicyStore,
    ScorerRef,
};
use autumn_harvest_agent::{AgentHarness, AgentProfile, Answered, BoxFuture, SweepTool};
use littleguy_core::ResearchConfig;

/// The agent kind of each source run.
pub const RESEARCH_KIND: &str = "research";

/// The system prompt of a source run.
pub const RESEARCH_SYSTEM: &str = "You read one source and answer the instruction about it. \
     Read the source with your tools. Answer in a few plain sentences, with no preamble. \
     When you cannot read the source, say why in one sentence.";

/// Gives the assistant `profile` the `sweep` tool, and gives `harness` the
/// research kind, its sampling policy and its scorer. With `workspace`, a
/// source run can read the files of the workspace too.
#[must_use]
pub fn apply(
    harness: AgentHarness,
    profile: AgentProfile,
    section: &ResearchConfig,
) -> (AgentHarness, AgentProfile) {
    let Some(setting) = setting(section) else {
        // The scorer id is a constant of the engine, so this never runs.
        tracing::error!("the answered scorer id is not valid: the sweep tool is off");
        return (harness, profile);
    };
    let workspace = profile.workspace;
    let harness = harness
        .agent(item_profile(section, workspace))
        .scorer(Arc::new(Answered::new()))
        .sampling_policies(Arc::new(ConfigPolicies::new(section)));
    (harness, profile.sweep(setting))
}

/// The profile of one source run. It never writes: the policy allows only
/// read-only draws, and the profile says so too.
fn item_profile(section: &ResearchConfig, workspace: bool) -> AgentProfile {
    let steps = section
        .max_steps_per_item
        .map_or(u32::MAX, std::num::NonZeroU32::get);
    let mut profile = AgentProfile::new(RESEARCH_KIND)
        .system(RESEARCH_SYSTEM)
        .max_steps(steps)
        .read_only();
    if let Some(tokens) = section.max_tokens_per_item {
        profile = profile.max_total_tokens(tokens.get());
    }
    if workspace {
        profile = profile.workspace();
    }
    profile
}

/// The engine setting of the `sweep` tool. Each result marks the run with
/// `untrusted_input`, because a source run reads outside text. `None` only
/// when the engine refuses its own scorer id.
fn setting(section: &ResearchConfig) -> Option<SweepTool> {
    let scorer = ScorerRef::deterministic(Answered::ID)?;
    let ceiling = section
        .max_tokens_per_call
        .unwrap_or(std::num::NonZeroU32::MAX);
    let mut setting = SweepTool::new(RESEARCH_KIND, ceiling, scorer)
        .concurrency(section.concurrency.get())
        .flow_label(labels::UNTRUSTED_INPUT);
    if let Some(max) = section.max_items {
        setting = setting.max_items(max.get());
    }
    Some(setting)
}

/// The sampling policies of the config. The research kind runs single
/// draws, at most `concurrency` at the same time, with read-only tools and
/// scorers that call no model. Every other kind reads every default. The
/// config is the only writer, so a set or a reset is refused.
#[derive(Debug)]
pub struct ConfigPolicies {
    research: SamplingPolicy,
}

impl ConfigPolicies {
    /// The policies of `section`.
    #[must_use]
    pub fn new(section: &ResearchConfig) -> Self {
        Self {
            research: SamplingPolicy {
                max_concurrent_draws: section.concurrency.get(),
                ..SamplingPolicy::default()
            },
        }
    }

    /// The policy of `kind`.
    #[must_use]
    pub fn policy(&self, kind: &str) -> SamplingPolicy {
        if kind == RESEARCH_KIND {
            self.research.clone()
        } else {
            SamplingPolicy::default()
        }
    }
}

/// Why a change is refused: the config owns the policies.
const fn config_owns() -> PolicyStoreError {
    PolicyStoreError::InvalidActor {
        reason: "the [research] section of the config sets the sampling policy",
    }
}

impl SamplingPolicyStore for ConfigPolicies {
    fn read<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Result<PolicyRead, PolicyStoreError>> {
        let read = PolicyRead {
            policy: self.policy(kind),
            revision: 0,
        };
        Box::pin(std::future::ready(Ok(read)))
    }

    fn set<'a>(
        &'a self,
        _kind: &'a str,
        _policy: &'a SamplingPolicy,
        _actor: &'a str,
    ) -> BoxFuture<'a, Result<PolicyChange, PolicyStoreError>> {
        Box::pin(std::future::ready(Err(config_owns())))
    }

    fn reset<'a>(
        &'a self,
        _kind: &'a str,
        _actor: &'a str,
    ) -> BoxFuture<'a, Result<PolicyChange, PolicyStoreError>> {
        Box::pin(std::future::ready(Err(config_owns())))
    }

    fn audit<'a>(
        &'a self,
        _kind: &'a str,
        _limit: u32,
    ) -> BoxFuture<'a, Result<Vec<PolicyAudit>, PolicyStoreError>> {
        Box::pin(std::future::ready(Ok(Vec::new())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(text: &str) -> ResearchConfig {
        littleguy_core::Config::parse(&format!("owner = \"mark\"\n[research]\n{text}"))
            .unwrap()
            .research
            .unwrap()
    }

    #[test]
    fn the_research_policy_is_single_read_only_draws_at_the_concurrency() {
        let policies = ConfigPolicies::new(&section("max_tokens_per_call = 1000\nconcurrency = 8"));
        let research = policies.policy(RESEARCH_KIND);
        assert_eq!(research.max_concurrent_draws, 8);
        assert!(research.strategies.is_empty(), "single draws only");
        assert_eq!(research.max_draws, 1);
        assert_eq!(
            research.draw_effects,
            std::collections::BTreeSet::from([autumn_harvest_agent::ToolEffect::ReadOnly])
        );
        assert!(research.judge_models.is_empty(), "no judge model");
        assert_eq!(research.max_answer_retries, 0);
        research.validate().unwrap();
        assert_eq!(policies.policy("assistant"), SamplingPolicy::default());
    }

    #[tokio::test]
    async fn the_config_is_the_only_writer_of_a_policy() {
        let policies = ConfigPolicies::new(&section("max_tokens_per_call = 1000"));
        let read = policies.read(RESEARCH_KIND).await.unwrap();
        assert_eq!(read.revision, 0);
        let wide = SamplingPolicy {
            max_concurrent_draws: 16,
            ..SamplingPolicy::default()
        };
        assert!(matches!(
            policies.set(RESEARCH_KIND, &wide, "op").await,
            Err(PolicyStoreError::InvalidActor { .. })
        ));
        assert!(policies.reset(RESEARCH_KIND, "op").await.is_err());
        assert_eq!(policies.audit(RESEARCH_KIND, 10).await.unwrap(), Vec::new());
    }

    #[test]
    fn the_setting_carries_the_owner_bounds_and_marks_the_run() {
        let set = setting(&section(
            "max_tokens_per_call = 640000\nconcurrency = 8\nmax_items = 40",
        ))
        .unwrap();
        assert_eq!(set.kind, RESEARCH_KIND);
        assert_eq!(set.max_total_tokens.get(), 640_000);
        assert_eq!(
            set.item_concurrency().map(std::num::NonZeroU32::get),
            Some(8)
        );
        assert_eq!(set.item_bound(), 40);
        assert_eq!(set.scorer.id.as_str(), Answered::ID);
        assert!(set.flow_labels.contains(labels::UNTRUSTED_INPUT));
        let bare = setting(&section("")).unwrap();
        assert_eq!(
            bare.max_total_tokens,
            std::num::NonZeroU32::MAX,
            "no ceiling of its own"
        );
        assert_eq!(
            bare.item_bound(),
            autumn_harvest_agent::sampling::MAX_SWEEP_ITEMS
        );
    }

    #[test]
    fn a_source_run_is_read_only_and_takes_the_item_bounds() {
        let bounded = item_profile(
            &section(
                "max_tokens_per_call = 1000\nmax_tokens_per_item = 300\nmax_steps_per_item = 4",
            ),
            true,
        );
        assert_eq!(bounded.kind, RESEARCH_KIND);
        assert!(bounded.read_only);
        assert_eq!(bounded.max_steps, 4);
        assert_eq!(bounded.max_total_tokens, Some(300));
        assert!(bounded.workspace);
        assert!(bounded.sweep.is_none(), "a source run never sweeps again");
        let open = item_profile(&section("max_tokens_per_call = 1000"), false);
        assert_eq!(open.max_steps, u32::MAX, "no invented step bound");
        assert_eq!(open.max_total_tokens, None, "no invented token cap");
        assert!(!open.workspace);
    }
}
