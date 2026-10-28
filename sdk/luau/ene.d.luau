-- ene-plugin contract version 1. The runner exposes ene.contract_version.
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
export type GenerationRequest = {
    endpoint:string, options:any, model:string, preamble:string,
    messages:{{role:string,text:string}}, max_tokens:number,
    tools:{{name:string,description:string,parameters:any}}?,
}
export type DesktopOperation =
    {type:"clipboard_read"} | {type:"clipboard_write",text:string} |
    {type:"notify",title:string,body:string} | {type:"select_file"} |
    {type:"capture_screen"} | {type:"avatar",action:CharacterAction} |
    {type:"read_resource",handle:string,offset:number,length:number} |
    {type:"close_resource",handle:string}
export type ResourceHandle = {id:string,media_type:string,size:number,expires_at_ms:number}
export type ServiceRequest =
    {type:"configuration"} | {type:"log",message:string} |
    {type:"state_read",key:string} | {type:"state_write",key:string,value:any} |
    {type:"http_open",request:HttpRequest} | {type:"http_read",handle:string} |
    {type:"http_close",handle:string} | {type:"models",request:GenerationRequest} |
    {type:"tool",name:string,arguments:any} | {type:"character",action:CharacterAction} |
    {type:"desktop",operation:DesktopOperation}
export type SDK = {contract_version:number,service:(ServiceRequest)->any,emit:(any)->()}
return nil