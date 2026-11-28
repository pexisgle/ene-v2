use crate::approval::ApprovalCommands as _;
use crate::plugins::PluginCommands as _;

use super::*;

#[allow(refining_impl_trait)]
impl w::SettingsService for Api {
    unary!(reset_history, w::Empty, w::Empty, |actor, _req| {
        crate::server::history::request(&actor.state)?;
        actor.host.shutdown.cancel();
        Ok(w::Empty::default())
    });

    unary!(get, w::Empty, w::Settings, |actor, _req| actor
        .settings()
        .await);
    unary!(
        put_connection,
        w::ConnectionChange,
        w::Connection,
        |actor, req| actor.put_connection(req).await
    );
    unary!(
        delete_connection,
        w::DeleteRequest,
        w::Empty,
        |actor, req| actor.delete_record("connection", req).await
    );
    unary!(
        set_credential,
        w::CredentialChange,
        w::Empty,
        |actor, req| actor.set_credential(req).await
    );
    unary!(put_policy, w::PolicyChange, w::Policy, |actor, req| actor
        .put_policy(req)
        .await);
    unary!(delete_rule, w::DeleteRequest, w::Empty, |actor, req| actor
        .delete_record("rule", req)
        .await);
    unary!(answer, w::ApprovalAnswer, w::Empty, |actor, req| actor
        .answer(req)
        .await);
    unary!(put_plugin, w::PluginChange, w::Plugin, |actor, req| actor
        .put_plugin(req)
        .await);
    unary!(delete_plugin, w::DeleteRequest, w::Empty, |actor, req| {
        actor.delete_record("plugin", req).await
    });
    unary!(audit_history, w::AuditQuery, w::Audit, |actor, req| {
        Ok(w::Audit {
            items: store::audit_history(&actor.db, req.before, req.limit).await?,
            ..Default::default()
        })
    });

    async fn models(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::ModelList> {
        let id = request.to_owned_message().id;
        let (models, connection) = self
            .0
            .call(move |actor| {
                Box::pin(async move { Ok((actor.models.clone(), actor.connection(&id).await?)) })
            })
            .await
            .map_err(rpc)?;
        let model = models
            .model(w::ModelSelection::default(), connection)
            .await
            .map_err(rpc)?;
        let ids = model.models().await.map_err(rpc)?;
        Response::ok(w::ModelList {
            ids,
            ..Default::default()
        })
    }

    async fn mcp_tools(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::ModelList> {
        let id = request.to_owned_message().id;
        let (models, connection) = self
            .0
            .call(move |actor| {
                Box::pin(async move { Ok((actor.models.clone(), actor.connection(&id).await?)) })
            })
            .await
            .map_err(rpc)?;
        let cancel = self.0.shutdown.child_token();
        let _guard = cancel.clone().drop_guard();
        let mcp = Mcp::open(connection, &models, &cancel).await.map_err(rpc)?;
        let tools = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            mcp.service.list_all_tools(),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| ConnectError::unavailable("configured MCP unavailable"))?;
        let mut ids: Vec<_> = tools
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        ids.sort();
        ids.dedup();
        Response::ok(w::ModelList {
            ids,
            ..Default::default()
        })
    }

    async fn probe(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ProbeRequest>,
    ) -> ServiceResult<w::Empty> {
        let request = request.to_owned_message();
        let (models, connection) = self
            .0
            .call(move |actor| {
                Box::pin(async move {
                    Ok((
                        actor.models.clone(),
                        actor.connection(&request.connection_id).await?,
                    ))
                })
            })
            .await
            .map_err(rpc)?;
        if matches!(
            connection.kind.as_known(),
            Some(w::ProviderKind::McpHttp | w::ProviderKind::McpStdio)
        ) {
            let cancel = self.0.shutdown.child_token();
            let _guard = cancel.clone().drop_guard();
            let mcp = Mcp::open(connection, &models, &cancel).await.map_err(rpc)?;
            mcp.service
                .list_all_tools()
                .await
                .map_err(|_| ConnectError::unavailable("configured MCP unavailable"))?;
        } else if let Some(selection) = request.model.into_option() {
            if selection.connection_id != connection.id {
                return Err(ConnectError::invalid_argument(
                    "model does not belong to this connection",
                ));
            }
            let model = models.model(selection, connection).await.map_err(rpc)?;
            let cancel = self.0.shutdown.child_token();
            let _guard = cancel.clone().drop_guard();
            use w::model_selection::Role;
            match model.selection.role.as_known() {
                Some(Role::Synthesis) => {
                    crate::voice::synthesize(&model, "こんにちは", request.voice, 1., &cancel)
                        .await
                        .map_err(rpc)?;
                }
                Some(Role::Transcription) => {
                    crate::voice::transcribe(&model, 24_000, vec![0; 24_000], &cancel)
                        .await
                        .map_err(rpc)?;
                }
                Some(Role::Embedding) => {
                    model
                        .embed(vec!["connection probe".into()], &cancel)
                        .await
                        .map_err(rpc)?;
                }
                _ => {
                    model.text("Reply briefly.", "OK").await.map_err(rpc)?;
                }
            }
        } else {
            let model = models
                .model(w::ModelSelection::default(), connection)
                .await
                .map_err(rpc)?;
            model.models().await.map_err(rpc)?;
        }
        Response::ok(w::Empty::default())
    }
}
