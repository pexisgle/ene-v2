//! One authorization boundary for native credentials and same-origin web sessions.
use super::{error::ApiError, state::App};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{Mutex, watch},
    time::Instant,
};
use uuid::Uuid;
#[derive(Clone)]
pub(crate) enum Principal {
    Owner,
    Device(String, watch::Receiver<()>),
    Browser(WebSession),
}
#[derive(Clone)]
pub(crate) struct WebSession {
    pub id: String,
    pub expires: Instant,
    pub revoked: watch::Receiver<bool>,
}
#[derive(Clone, Default)]
pub(crate) struct Sessions(Arc<Mutex<SessionMap>>);
type SessionMap = HashMap<String, (WebSession, watch::Sender<bool>)>;
impl Sessions {
    pub async fn create(&self) -> Result<WebSession, ApiError> {
        let mut sessions = self.0.lock().await;
        sessions.retain(|_, (s, sender)| {
            if s.expires <= Instant::now() {
                sender.send_replace(true);
                return false;
            }
            !*s.revoked.borrow()
        });
        if sessions.len() >= 128 {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "resource_limit",
                "セッション数の上限です",
            ));
        }
        let (sender, receiver) = watch::channel(false);
        let session = WebSession {
            id: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            expires: Instant::now() + Duration::from_secs(8 * 3600),
            revoked: receiver,
        };

        sessions.insert(session.id.clone(), (session.clone(), sender));
        Ok(session)
    }
    pub async fn get(&self, id: &str) -> Option<WebSession> {
        self.0
            .lock()
            .await
            .get(id)
            .filter(|(s, _)| s.expires > Instant::now() && !*s.revoked.borrow())
            .map(|(s, _)| s.clone())
    }
    pub async fn revoke(&self, id: &str) {
        if let Some((_, sender)) = self.0.lock().await.remove(id) {
            sender.send_replace(true);
        }
    }
}
pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
pub(crate) fn owner(app: &App, headers: &HeaderMap) -> bool {
    bearer(headers).is_some_and(|v| bool::from(v.as_bytes().ct_eq(app.token.as_bytes())))
}
pub(crate) fn session_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|v| v.strip_prefix("ene_session="))
}
pub(crate) fn same_origin(app: &App, headers: &HeaderMap) -> bool {
    let Some(expected) = &app.web_origin else {
        return false;
    };
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin == expected.as_ref())
}
pub(crate) async fn authenticate(
    State(app): State<App>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let principal = if owner(&app, headers) {
        Some(Principal::Owner)
    } else if let Some(token) = bearer(headers) {
        if let Some(id) = app.devices.authenticate(token).await {
            app.devices
                .changes(&id)
                .await
                .ok()
                .map(|changed| Principal::Device(id, changed))
        } else {
            None
        }
    } else if same_origin(&app, headers) {
        if let Some(id) = session_id(headers) {
            app.sessions.get(id).await.map(Principal::Browser)
        } else {
            None
        }
    } else {
        None
    };
    let Some(principal) = principal else {
        if request.uri().path().starts_with("/rpc/") {
            return connectrpc::ConnectError::unauthenticated("認証が必要です")
                .into_http_response(headers)
                .map(axum::body::Body::new);
        }
        return ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "認証が必要です")
            .into_response();
    };
    if matches!(principal, Principal::Device(_, _))
        && !matches!(
            request.uri().path(),
            "/rpc/ene.host.v1.HostService/GetSnapshot"
                | "/rpc/ene.host.v1.HostService/Subscribe"
                | "/rpc/ene.host.v1.HostService/ListCompanions"
                | "/rpc/ene.host.v1.HostService/GetCompanion"
                | "/rpc/ene.host.v1.HostService/GetHistory"
                | "/rpc/ene.host.v1.HostService/GetMessage"
                | "/rpc/ene.host.v1.HostService/SendMessage"
                | "/rpc/ene.host.v1.HostService/Stop"
                | "/rpc/ene.host.v1.HostService/GetAvailability"
                | "/rpc/ene.host.v1.HostService/PrepareAudio"
                | "/rpc/ene.host.v1.DeviceService/Subscribe"
                | "/rpc/ene.host.v1.DeviceService/CompleteOperation"
                | "/rpc/ene.host.v1.DeviceService/GetOperation"
                | "/rpc/ene.host.v1.DeviceService/Activate"
                | "/audio"
        )
    {
        return connectrpc::ConnectError::permission_denied("管理権限が必要です")
            .into_http_response(headers)
            .map(axum::body::Body::new);
    }
    // Bearer WebSocket use also rejects an explicitly foreign browser Origin.
    if request.uri().path() == "/audio"
        && headers.contains_key(header::ORIGIN)
        && !same_origin(&app, headers)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    request.extensions_mut().insert(principal);
    next.run(request).await
}
pub(crate) fn own_device(
    ctx: &connectrpc::RequestContext,
    id: &str,
) -> Result<(), connectrpc::ConnectError> {
    match ctx.extensions().get::<Principal>() {
        Some(Principal::Device(own, _)) if own != id => Err(
            connectrpc::ConnectError::permission_denied("別の端末は操作できません"),
        ),
        Some(_) => Ok(()),
        None => Err(connectrpc::ConnectError::unauthenticated("認証が必要です")),
    }
}
pub(crate) async fn cancelled(principal: Principal) {
    if let Principal::Browser(mut session) = principal {
        if *session.revoked.borrow() {
            return;
        }
        tokio::select! {_=session.revoked.changed()=>{},_=tokio::time::sleep_until(session.expires)=>{}}
    } else if let Principal::Device(_, mut changed) = principal {
        if changed.has_changed().unwrap_or(true) {
            return;
        }
        let _ = changed.changed().await;
    } else {
        std::future::pending::<()>().await;
    }
}
