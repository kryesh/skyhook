//! Compile-time coverage for Skyhook's intended domain-oriented public paths.

use skyhook::{
    agent::{
        Harness, HarnessBuilder, SessionHandle,
        handle::SessionHandle as DomainSessionHandle,
        harness::{Harness as DomainHarness, HarnessBuilder as DomainHarnessBuilder},
        interaction::{Question, QuestionHandler, RuntimeEvent},
        profile::AgentProfile,
        todo::{TodoItem, TodoStatus},
    },
    config::{Config, ProviderConfig},
    job::{JobManager, JobState},
    media::ImageReference,
    provider::{
        Provider, ProviderError, ResponseHandle,
        backends::flux::{FluxProvider, OpenAiApi},
        profile::ModelProfile,
        protocol::{Message, ModelRequest, ResponseChunk},
    },
    session::{EventRecord, SessionEvent, SessionStore},
    tool::{
        ToolContext, ToolOutput, ToolRegistry, ToolRegistryBuilder,
        executor::ToolExecutor,
        policy::{Policy, ToolEffect},
    },
};

#[test]
fn canonical_public_modules_are_accessible() {
    fn same_type<T>() {}
    fn provider_is_object_safe(_: &dyn Provider) {}
    fn policy_is_object_safe(_: &dyn Policy) {}
    fn questions_are_object_safe(_: &dyn QuestionHandler) {}

    same_type::<Harness>();
    same_type::<HarnessBuilder>();
    same_type::<SessionHandle>();
    same_type::<DomainHarness>();
    same_type::<DomainHarnessBuilder>();
    same_type::<DomainSessionHandle>();
    same_type::<Question>();
    same_type::<RuntimeEvent>();
    same_type::<AgentProfile>();
    same_type::<TodoItem>();
    same_type::<TodoStatus>();
    same_type::<Config>();
    same_type::<ProviderConfig>();
    same_type::<JobManager>();
    same_type::<JobState>();
    same_type::<ImageReference>();
    same_type::<ProviderError>();
    same_type::<Box<dyn ResponseHandle>>();
    same_type::<FluxProvider>();
    same_type::<OpenAiApi>();
    same_type::<ModelProfile>();
    same_type::<Message>();
    same_type::<ModelRequest>();
    same_type::<ResponseChunk>();
    same_type::<EventRecord>();
    same_type::<SessionEvent>();
    same_type::<SessionStore>();
    same_type::<ToolContext>();
    same_type::<ToolOutput>();
    same_type::<ToolRegistry>();
    same_type::<ToolRegistryBuilder>();
    same_type::<ToolExecutor>();
    same_type::<ToolEffect>();

    let _ = provider_is_object_safe;
    let _ = policy_is_object_safe;
    let _ = questions_are_object_safe;
}
