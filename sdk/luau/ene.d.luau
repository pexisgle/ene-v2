-- Runtime globals. Data variants are generated in docs/plugin-schema.json.
export type HttpRequest = {method:string,url:string,headers:{[string]:string}?,body_base64:string?}
export type HttpChunk = {handle:string,status:number,headers:{[string]:string},data_base64:string,done:boolean}
export type CharacterAction = {type:"speak",text:string} | {type:"expression",name:string,duration_ms:number} | {type:"motion",asset:string,duration_ms:number}
export type ServiceRequest = {type:"configuration"} | {type:"log",message:string} | {type:"state_read",key:string} | {type:"state_write",key:string,value:any} | {type:"http_open",request:HttpRequest} | {type:"http_read",handle:string} | {type:"http_close",handle:string}
export type SDK = {service:(ServiceRequest)->any,emit:(any)->()}
return nil
