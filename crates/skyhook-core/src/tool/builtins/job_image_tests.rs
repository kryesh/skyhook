//! End-to-end attachment coverage through the tool executor and JavaScript bridge.
use std::sync::{Arc, OnceLock};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;

use crate::{
    identity::AgentId,
    job::{JobManager, JobOutcome, JobSpec},
    media::ImageReference,
    provider::protocol::{Message, ModelRequest, ToolResult},
    session::SessionStore,
    test_support::TestRuntime,
    tool::{
        ToolOutput, ToolRegistryBuilder,
        executor::ToolExecutor,
        policy::{AllowAll, Capability, CapabilitySet},
    },
};

const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nattachment test";

fn executor(
    store: SessionStore,
    jobs: JobManager,
    root: &std::path::Path,
) -> (ToolExecutor, Arc<OnceLock<ToolExecutor>>) {
    let mut builder = ToolRegistryBuilder::default();
    super::super::filesystem::register(&mut builder, store).unwrap();
    super::register(&mut builder, jobs.clone()).unwrap();
    let slot = Arc::new(OnceLock::new());
    super::super::install_script_tool(&mut builder, Arc::downgrade(&slot)).unwrap();
    let executor = ToolExecutor::new(builder.build(), Arc::new(AllowAll), jobs, root.into());
    assert!(slot.set(executor.clone()).is_ok());
    (executor, slot)
}

async fn assert_hydrated(store: &SessionStore, images: Vec<ImageReference>) {
    assert_eq!(images.len(), 1);
    assert!(
        images[0].data_base64.is_none(),
        "journal references stay payload-free"
    );
    let mut request = ModelRequest {
        model: "image-test".into(),
        system: vec![],
        tools: vec![],
        response_schema: None,
        reasoning: None,
        max_output_tokens: None,
        correlation: None,
        messages: vec![Message::Tool(vec![ToolResult {
            call_id: "output".into(),
            name: "job_output".into(),
            result: json!({}),
            images,
            is_error: false,
        }])],
    };
    store.hydrate_model_request(&mut request).await.unwrap();
    let Message::Tool(results) = &request.messages[0] else {
        panic!("tool results")
    };
    assert_eq!(
        results[0].images[0].data_base64.as_deref(),
        Some(STANDARD.encode(IMAGE).as_str())
    );
}

#[tokio::test]
async fn background_image_output_attaches_directly_and_through_javascript() {
    let runtime = TestRuntime::new().await;
    tokio::fs::write(runtime.root.path().join("image.png"), IMAGE)
        .await
        .unwrap();
    let (executor, _slot) = executor(
        runtime.store.clone(),
        runtime.jobs.clone(),
        runtime.root.path(),
    );
    // read itself is foreground-only; a background script is the supported way
    // to read an image asynchronously and retrieve its saved output later.
    let launched = executor
        .execute_model(
            runtime.agent.clone(),
            "script",
            json!({"source":"return await tool.read({path:'image.png'});","bg":true}),
            None,
        )
        .await
        .unwrap();
    assert!(launched.background);
    runtime.jobs.wait(launched.job, None, true).await.unwrap();
    for _ in 0..2 {
        let output = executor
            .execute_model(
                runtime.agent.clone(),
                "job_output",
                json!({"job":launched.job}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(output.output.value["state"], "completed");
        // Returning a tool result preserves its originating read job view.
        assert_eq!(output.output.value["result"]["value"]["tool"], "read");
        assert!(
            output.output.value["result"]["value"]["result"]["image"].is_object(),
            "{}",
            output.output.value
        );
        assert_eq!(output.output.value["result"]["console"], "");
        assert!(output.output.value.get("console").is_none());
        assert_hydrated(&runtime.store, output.output.images).await;
    }
    let source = format!("return await tool.job({}).output();", launched.job);
    let output = executor
        .execute_model(
            runtime.agent.clone(),
            "script",
            json!({"source":source}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(output.output.value["result"]["console"], "");
    assert_eq!(
        output.output.value["result"]["value"]["id"],
        launched.job.get()
    );
    assert!(
        output.output.value["result"]["value"]["result"]["value"]["result"]["image"].is_object()
    );
    assert_hydrated(&runtime.store, output.output.images).await;

    // A background script that retrieves an existing image must itself retain
    // the attachment, not merely the nested job's JSON metadata.
    let script = executor
        .execute_model(
            runtime.agent.clone(),
            "script",
            json!({"source":format!("return await tool.job({}).output();", launched.job), "bg":true}),
            None,
        )
        .await
        .unwrap();
    runtime.jobs.wait(script.job, None, true).await.unwrap();
    let output = executor
        .execute_model(
            runtime.agent.clone(),
            "job_output",
            json!({"job":script.job}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(output.output.value["state"], "completed");
    assert_hydrated(&runtime.store, output.output.images).await;
}

#[tokio::test]
async fn image_output_text_selections_do_not_attach_and_invalid_queries_fail() {
    let runtime = TestRuntime::new().await;
    tokio::fs::write(runtime.root.path().join("image.png"), IMAGE)
        .await
        .unwrap();
    let (executor, _slot) = executor(
        runtime.store.clone(),
        runtime.jobs.clone(),
        runtime.root.path(),
    );
    let read = executor
        .execute_model(
            runtime.agent.clone(),
            "read",
            json!({"path":"image.png"}),
            None,
        )
        .await
        .unwrap();
    assert_hydrated(&runtime.store, read.output.images).await;
    for selection in [
        json!({"field":"/result"}),
        json!({"field":""}),
        json!({"start":1}),
        json!({"limit":100}),
        json!({"offset":0}),
        json!({"context":0}),
        json!({"pattern":"image"}),
        json!({"pattern":"image", "context":1}),
    ] {
        let mut query = selection;
        query["job"] = json!(read.job);
        let output = executor
            .execute_model(runtime.agent.clone(), "job_output", query.clone(), None)
            .await
            .unwrap();
        assert!(output.output.images.is_empty(), "{query}");
        // The job binding supplies the ID; the object form accepts only options.
        let mut options = query.clone();
        options.as_object_mut().unwrap().remove("job");
        let source = format!("return await tool.job({}).output({options});", read.job);
        let output = executor
            .execute_model(
                runtime.agent.clone(),
                "script",
                json!({"source":source}),
                None,
            )
            .await
            .unwrap();
        assert!(output.output.images.is_empty(), "{query}");
    }
    for query in [
        json!({"job":read.job,"limit":0}),
        json!({"job":read.job,"pattern":"["}),
        json!({"job":999999}),
    ] {
        assert!(
            executor
                .execute_model(runtime.agent.clone(), "job_output", query, None)
                .await
                .is_err()
        );
    }
    // A denied image-producing call never creates a retrievable attachment.
    let mut capabilities = CapabilitySet::default();
    capabilities.remove(Capability::Read);
    assert!(
        executor
            .clone()
            .with_capabilities(capabilities)
            .execute_model(
                runtime.agent.clone(),
                "read",
                json!({"path":"image.png"}),
                None
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn retrieved_images_survive_resume_including_failed_tool_output() {
    let runtime = TestRuntime::new().await;
    let image = runtime
        .store
        .import_blob(IMAGE, "image.png".into(), "image/png".into())
        .await
        .unwrap();
    let job = runtime
        .jobs
        .create(JobSpec::test(runtime.agent.clone(), "partial-image"))
        .await
        .unwrap()
        .id;
    runtime
        .jobs
        .finish(
            job,
            JobOutcome::Failed {
                message: "failed after producing an image".into(),
                output: Some(ToolOutput::new(json!({"image":image})).with_images(vec![image])),
                denial: None,
            },
        )
        .await
        .unwrap();
    let id = runtime.store.id();
    drop(runtime.jobs);
    drop(runtime.store);
    let (store, records) = SessionStore::open(&runtime.root.path().join("sessions"), id)
        .await
        .unwrap();
    let jobs = JobManager::restore(store.clone(), &records).await.unwrap();
    let agent = AgentId::root(id);
    let (executor, _slot) = executor(store.clone(), jobs, runtime.root.path());
    let output = executor
        .execute_model(agent.clone(), "job_output", json!({"job":job}), None)
        .await
        .unwrap();
    assert_eq!(output.output.value["state"], "failed");
    assert_hydrated(&store, output.output.images).await;
    let source = format!("return await tool.job({job}).output();");
    let output = executor
        .execute_model(agent, "script", json!({"source":source}), None)
        .await
        .unwrap();
    assert_hydrated(&store, output.output.images).await;
}
