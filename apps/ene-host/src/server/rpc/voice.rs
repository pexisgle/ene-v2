//! Voice service contract, unimplemented in this phase.
use super::Services;
use connectrpc::{
    ConnectError, InboundStream, RequestContext, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_rpc as w;

#[allow(refining_impl_trait)]
impl w::VoiceService for Services {
    async fn stream_audio_input(
        &self,
        _: RequestContext,
        _: InboundStream<w::AudioInputChunk>,
    ) -> ServiceResult<ServiceStream<w::TranscriptionEvent>> {
        Err(ConnectError::unimplemented(
            "voice service is not implemented",
        ))
    }

    async fn subscribe_audio_output(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::SubscribeAudioRequest>,
    ) -> ServiceResult<ServiceStream<w::AudioOutputEvent>> {
        Err(ConnectError::unimplemented(
            "voice service is not implemented",
        ))
    }

    async fn report_playback_status(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::PlaybackStatusReport>,
    ) -> ServiceResult<w::ReportPlaybackStatusResponse> {
        Err(ConnectError::unimplemented(
            "voice service is not implemented",
        ))
    }
}
