//! Work status: the sessions the host runs for the shown companion, their runs,
//! and the questions a run is waiting on.
//!
//! The window does not start work. The adopted design routes a work request
//! through the conversation with the companion, so this panel shows what exists
//! and lets the operator answer, stop or close it.
//!
//! `ListWorks` returns complete `WorkView`s, runs and pending inquiries included,
//! so one call is the whole picture and the window keeps no second copy.

use super::{Desktop, decode};
use crate::update::Update;
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::Context;

impl Desktop {
    /// Fetches every work view for the shown companion.
    pub(crate) fn load_works(&mut self, cx: &mut Context<Self>) {
        let Some(companion_id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        if !self.live() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        if self.model.works_companion.as_deref() != Some(companion_id.as_str()) {
            // A different companion's sessions must not be shown as this one's.
            self.model.works.clear();
            self.model.works_companion = Some(companion_id.clone());
        }
        self.model.works_loading = true;
        let shown = companion_id.clone();
        self.request(
            "作業一覧の取得",
            async move {
                let response = host
                    .work
                    .list_works(w::ListWorksRequest {
                        companion_id,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |works| Update::Works {
                companion_id: shown,
                items: works.items,
            },
        );
        cx.notify();
    }

    pub(crate) fn answer(&mut self, work_id: String, request_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let answer = self
            .drafts
            .get(&format!("answer:{request_id}"))
            .map(|draft| draft.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        if answer.is_empty() {
            return;
        }
        self.request(
            "質問への回答",
            async move {
                host.work
                    .answer_inquiry(w::AnswerInquiryRequest {
                        work_id,
                        request_id,
                        answer,
                        client_id: String::new(),
                        ..Default::default()
                    })
                    .await
            },
            |_response| Update::Resnapshot,
        );
        cx.notify();
    }

    pub(crate) fn stop_work(&mut self, work_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "作業の停止",
            async move {
                host.work
                    .stop_work(w::StopWorkRequest {
                        work_id,
                        ..Default::default()
                    })
                    .await
            },
            |_response| Update::Resnapshot,
        );
        cx.notify();
    }
}
