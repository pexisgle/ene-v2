//! Voice service contract. This alpha ships no STT/TTS engine, so every
//! method reports the capability as unavailable instead of faking success.
use super::Services;
use connectrpc::{
    ConnectError, InboundStream, RequestContext, ServiceRequest, ServiceResult, ServiceStream,
};
use ene_rpc as w;

const UNAVAILABLE: &str = "voice requires a configured STT/TTS connection";

#[allow(refining_impl_trait)]
impl w::VoiceService for Services {
    async fn stream_audio_input(
        &self,
        _: RequestContext,
        _: InboundStream<w::AudioInputChunk>,
    ) -> ServiceResult<ServiceStream<w::TranscriptionEvent>> {
        Err(ConnectError::unimplemented(UNAVAILABLE))
    }

    async fn subscribe_audio_output(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::SubscribeAudioRequest>,
    ) -> ServiceResult<ServiceStream<w::AudioOutputEvent>> {
        Err(ConnectError::unimplemented(UNAVAILABLE))
    }

    async fn report_playback_status(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::PlaybackStatusReport>,
    ) -> ServiceResult<w::ReportPlaybackStatusResponse> {
        Err(ConnectError::unimplemented(UNAVAILABLE))
    }
}
