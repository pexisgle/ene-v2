-- ene-plugin contract version 2. The runner exposes ene.contract_version.
-- Model/metadata fields follow generated protobuf JSON: camelCase field names,
-- uint64 values encoded as strings, and StringList messages wrapping `values`.
export type UInt64 = string | number
export type StringList = {values:{string}?}
export type ModelMetadata = {
    source:string?, scope:string?, contextWindow:UInt64?, maxInputTokens:UInt64?,
    maxOutputTokens:UInt64?, inputModalities:StringList?, outputModalities:StringList?,
    supportedParameters:StringList?, fieldSources:{[string]:string}?,
}
export type Model = {id:string, name:string, metadata:ModelMetadata?}
export type HttpRequest = {method:string,url:string,headers:{[string]:string}?,body_base64:string?}
export type HttpChunk = {handle:string,status:number,headers:{[string]:string},data_base64:string,done:boolean}
export type CharacterAction = {type:"speak",text:string} | {type:"expression",name:string,duration_ms:number} | {type:"motion",asset:string,duration_ms:number}
export type CharacterTarget = {device_id:string,character_id:string}
export type WindowId = {device_id:string,generation:number,id:string}
-- Physical desktop pixels, not avatar logical coordinates.
export type WindowBounds = {x:number,y:number,width:number,height:number}
export type WindowInfo = {id:WindowId,application:string,title:string?,bounds:WindowBounds?}
export type WindowEventKind = "opened" | "closed" | "focused" | "unfocused" | "moved" | "resized" | "minimized" | "restored"
export type WindowEvent = {sequence:number,kind:WindowEventKind,window:WindowInfo}
export type WindowSubscription = {applications:{string},events:{WindowEventKind},include_title:boolean?}
export type WindowCapabilities = {events:{WindowEventKind},titles:boolean,bounds:boolean}
export type DesktopCapabilities = {device_id:string,generation:number,services:{string},windows:WindowCapabilities?}
export type CharacterEvent =
    {type:"activated"} | {type:"deactivated"} | {type:"conversation_started"} |
    {type:"conversation_finished",text:string} | {type:"task_changed",task_id:string,state:string} |
    {type:"timer",id:string} | {type:"custom",name:string,data:any} |
    {type:"window",event:WindowEvent}
export type CharacterOperation = {type:"character",target:CharacterTarget,event:CharacterEvent}
export type CharacterActionEvent = {type:"character",target:CharacterTarget,action:CharacterAction}
export type GenerationRequest = {
    endpoint:string, options:any, model:string, preamble:string,
    messages:{{role:string,text:string}}, max_tokens:number,
    tools:{{name:string,description:string,parameters:any}}?,
}
export type DesktopOperation =
    {type:"clipboard_read"} | {type:"clipboard_write",text:string} |
    {type:"notify",title:string,body:string} | {type:"select_file"} |
    {type:"capture_screen"} | {type:"avatar",target:CharacterTarget,action:CharacterAction} |
    {type:"read_resource",handle:string,offset:number,length:number} |
    {type:"close_resource",handle:string}
export type ResourceHandle = {id:string,media_type:string,size:number,expires_at_ms:number}
export type ServiceRequest =
    {type:"configuration"} | {type:"log",message:string} |
    {type:"state_read",key:string} | {type:"state_write",key:string,value:any} |
    {type:"http_open",request:HttpRequest} | {type:"http_read",handle:string} |
    {type:"http_close",handle:string} | {type:"models",request:GenerationRequest} |
    {type:"tool",name:string,arguments:any} | {type:"character",target:CharacterTarget,action:CharacterAction} |
    {type:"desktop",operation:DesktopOperation}
export type SDK = {contract_version:number,service:(ServiceRequest)->any,emit:(any)->()}
return nil
