-- Protocol fixture only: PCM silence and a deterministic transcript, no speech engine.
return {
    initialize = function() return {formats={"pcm_s16le"}, cancellation=true} end,
    invoke = function(operation)
        assert(operation.type == "speech")
        local input = operation.input
        if input.type == "synthesize" then
            ene.emit({type="audio",chunk={utterance_id=input.utterance_id,generation=input.generation,sequence=0,format="pcm_s16le",sample_rate=16000,channels=1,data_base64="AAA=",done=true}})
        else
            ene.emit({type="transcript",text="reference transcript"})
        end
        return {}
    end,
}
