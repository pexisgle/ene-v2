use super::*;
#[tokio::test]
async fn a_work_session_runs_while_the_parent_keeps_talking_and_reports_once() {
    let temp = tempfile::tempdir().unwrap();
    let parent = parent_model(vec![
        vec![
            E::tool_call("start", "start_work", json!({"instruction": "整理して"})),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("started"), E::final_response_with_default_usage()],
        vec![
            E::text("作業が終わったよ"),
            E::final_response_with_default_usage(),
        ],
    ]);
    let worker = text_model("done");
    let runtime = Runtime::start_with(temp.path(), scripted(parent, worker, false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    send(&runtime, &companion, "work", "任せる").await;
    until(&runtime, &companion, "started").await;
    let seen = until(&runtime, &companion, "作業が終わったよ").await;
    assert!(
        seen.items
            .iter()
            .any(|message| message.text.contains("done"))
    );
    let works = runtime
        .call(|app| app.world().resource::<Store>().works())
        .await
        .unwrap();
    assert_eq!(works.len(), 1);
    assert_eq!(works[0].status, WorkStatus::Idle);
    assert!(works[0].reported_run.as_deref().is_some());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn work_questions_become_pending_inquiries_and_answers_resolve_them() {
    let temp = tempfile::tempdir().unwrap();
    let worker = parent_model(vec![
        vec![
            E::tool_call(
                "ask",
                "report",
                json!({"kind": "question", "content": "どちらにしますか"}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("了解"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(temp.path(), scripted(text_model("unused"), worker, false))
        .await
        .unwrap();
    let companion = default_companion(&runtime).await;
    let work = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                work::start(
                    app,
                    companion,
                    "inquiry".into(),
                    "確認して".into(),
                    String::new(),
                )
            }
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let _ = runtime.call(|_| Ok(())).await;
            let seen = history(&runtime, &companion, true).await;
            if seen
                .items
                .iter()
                .any(|message| message.text.contains("作業からの質問"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the question did not reach the parent");
    let view = runtime
        .call({
            let id = work.id.clone();
            move |app| work::view(app.world_mut(), &id)
        })
        .await
        .unwrap();
    assert_eq!(view.pending_inquiries.len(), 1);
    let request_id = view.pending_inquiries[0].request_id.clone();
    runtime
        .call({
            let id = work.id.clone();
            move |app| {
                work::answer_inquiry(
                    app,
                    w::AnswerInquiryRequest {
                        work_id: id,
                        request_id,
                        answer: "左".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    let view = runtime
        .call({
            let id = work.id.clone();
            move |app| work::view(app.world_mut(), &id)
        })
        .await
        .unwrap();
    assert!(view.pending_inquiries.is_empty());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn send_work_replays_the_same_request_key_without_delivering_twice() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("unused"), text_model("done"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let work = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                work::start(
                    app,
                    companion,
                    "replay".into(),
                    "始めて".into(),
                    String::new(),
                )
            }
        })
        .await
        .unwrap();
    let send = |key: &'static str| {
        let id = work.id.clone();
        move |app: &mut rig_ecs::prelude::App| {
            work::send(
                app,
                w::SendWorkRequest {
                    work_id: id,
                    request_key: key.into(),
                    text: "追加".into(),
                    ..Default::default()
                },
            )
        }
    };
    let first = runtime.call(send("work-key")).await.unwrap();
    let second = runtime.call(send("work-key")).await.unwrap();
    assert_eq!(first.request_id, second.request_id);
    let conflicting = runtime
        .call({
            let id = work.id.clone();
            move |app: &mut rig_ecs::prelude::App| {
                work::send(
                    app,
                    w::SendWorkRequest {
                        work_id: id,
                        request_key: "work-key".into(),
                        text: "違う内容".into(),
                        ..Default::default()
                    },
                )
            }
        })
        .await;
    assert!(conflicting.is_err());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn stopping_a_closed_work_from_the_tool_keeps_it_closed() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("unused"), text_model("done"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let work = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                work::start(
                    app,
                    companion,
                    "closed".into(),
                    "止める".into(),
                    String::new(),
                )
            }
        })
        .await
        .unwrap();
    runtime
        .call({
            let id = work.id.clone();
            move |app| work::close(app, &id)
        })
        .await
        .unwrap();
    let status = runtime
        .call({
            let companion = companion.clone();
            let id = work.id.clone();
            move |app| {
                let agent = chat::find_agent(app.world_mut(), &AgentKey::parent(&companion))
                    .ok_or_else(|| HostError::not_found("agent not found"))?;
                work::stop_tool(app.world_mut(), agent, &id, None)?;
                Ok(app.world().resource::<Store>().work(&id)?.unwrap().status)
            }
        })
        .await
        .unwrap();
    assert_eq!(status, WorkStatus::Closed);
    runtime.shutdown().await.unwrap();
}
