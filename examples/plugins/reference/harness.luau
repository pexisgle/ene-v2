-- A deliberately deterministic harness for contract tests; the Host performs operations.
local pending = nil
return {
    initialize = function() return {consultation=true, cancellation=true} end,
    invoke = function(operation)
        assert(operation.type == "harness")
        local input = operation.input
        local event
        if input.type == "cancel" then
            pending = nil
            event = {type="cancelled"}
        elseif input.type == "start" then
            assert(pending == nil, "task already active")
            pending = "model-1"
            event = {type="model_request", operation_id=pending, request={endpoint="https://example.com/api",options={},model="reference",preamble="",messages={{role="user",text=input.instruction}},max_tokens=64}}
        elseif input.type == "result" then
            assert(input.operation_id == pending, "wrong operation result")
            if pending == "model-1" then
                pending = "tool-1"
                event = {type="tool_request", operation_id=pending, name="reference.inspect", arguments={}}
            else
                pending = "question-1"
                event = {type="question", question_id=pending, text="参照タスクを完了しますか？"}
            end
        elseif input.type == "answer" then
            assert(input.question_id == pending, "wrong consultation")
            pending = nil
            event = {type="completed", report=input.answer}
        else error("unsupported input") end
        ene.emit({type="harness", event=event})
        return {}
    end,
}
