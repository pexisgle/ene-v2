return {
    initialize = function() return { events = true, state = true } end,
    invoke = function(operation)
        assert(operation.type == "character")
        local event = operation.event
        if event.type == "activated" or event.type == "conversation_finished" then
            local count = ene.service({type="state_read", key="visits"})
            if type(count) ~= "number" then count = 0 end
            count += 1
            ene.service({type="state_write", key="visits", value=count})
            ene.emit({type="character", action={type="expression", name="happy", duration_ms=1500}})
            return {visits=count}
        end
        return {}
    end,
}
