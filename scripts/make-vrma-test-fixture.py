#!/usr/bin/env python3
"""Original, redistributable VRMA fixture: offset source rig, travel, bob, turn, arm sweep."""
import copy
import json
import math
from pathlib import Path
import struct

root=Path(__file__).resolve().parents[1]
output=root/'target/vrma-fixtures'
output.mkdir(parents=True,exist_ok=True)
data=bytearray()
views=[]
accessors=[]
def floats(values,kind,components):
    offset=len(data)
    data.extend(struct.pack('<'+'f'*len(values),*values))
    views.append(dict(buffer=0,byteOffset=offset,byteLength=len(data)-offset))
    accessor=dict(bufferView=len(views)-1,componentType=5126,count=len(values)//components,type=kind)
    if kind=='SCALAR':accessor.update(min=[min(values)],max=[max(values)])
    accessors.append(accessor)
    return len(accessors)-1

def qz(angle):return [0,0,math.sin(angle/2),math.cos(angle/2)]
def qy(angle):return [0,math.sin(angle/2),0,math.cos(angle/2)]
time=floats([0,0.5,1,1.5,2],'SCALAR',1)
hips=floats([v for pose in [(0.01,0.7,0.03),(3.01,0.85,2.03),(0.01,0.7,0.03),(-2.99,0.6,-1.97),(0.01,0.7,0.03)] for v in pose],'VEC3',3)
arm=floats([v for a in [0.2,-1.2,0.2,1.2,0.2] for v in qz(a)],'VEC4',4)
turn=floats([v for a in [0,0.4,0,-0.4,0] for v in qy(a)],'VEC4',4)
fixture=dict(asset=dict(version='2.0',generator='ene original VRMA regression fixture'),extensionsUsed=['VRMC_vrm_animation'],extensions=dict(VRMC_vrm_animation=dict(specVersion='1.0',humanoid=dict(humanBones=dict(hips=dict(node=1),spine=dict(node=2),head=dict(node=3),leftUpperArm=dict(node=4))))),scene=0,scenes=[dict(nodes=[0])],nodes=[dict(name='source-origin',translation=[0.2,0.3,-0.1],rotation=qy(0.2),children=[1]),dict(name='hips',translation=[0.01,0.7,0.03],children=[2]),dict(name='spine',translation=[0,0.3,0],children=[3,4]),dict(name='head',translation=[0,0.3,0]),dict(name='leftUpperArm',translation=[0.1,0.2,0],rotation=qz(0.2))],buffers=[dict(byteLength=len(data))],bufferViews=views,accessors=accessors,animations=[dict(name='anchor-and-silhouette',samplers=[dict(input=time,output=a,interpolation='LINEAR') for a in [hips,arm,turn]],channels=[dict(sampler=0,target=dict(node=1,path='translation')),dict(sampler=1,target=dict(node=4,path='rotation')),dict(sampler=2,target=dict(node=1,path='rotation'))])])
def save(name,value,binary):
    encoded=json.dumps(value,separators=(',',':')).encode()
    encoded+=b' '*((-len(encoded))%4)
    binary+=b'\0'*((-len(binary))%4)
    glb=struct.pack('<4sII',b'glTF',2,28+len(encoded)+len(binary))+struct.pack('<I4s',len(encoded),b'JSON')+encoded+struct.pack('<I4s',len(binary),b'BIN\0')+binary
    (output/name).write_bytes(glb)
save('anchor-and-silhouette.vrma',fixture,bytes(data))
# A physically identical skeleton and animation expressed below a rotated,
# scaled parent. Its local translation curves have different axes/units.
def qmul(a,b):
    x,y,z,w=a;u,v,t,s=b
    return [w*u+x*s+y*t-z*v,w*v-x*t+y*s+z*u,w*t+x*v-y*u+z*s,w*s-x*u-y*v-z*t]
angle=0.6
qx=[math.sin(angle/2),0,0,math.cos(angle/2)]
qxinv=[-qx[0],0,0,qx[3]]
def inverse_position(v):
    x,y,z=v
    return [x/2,(math.cos(angle)*y+math.sin(angle)*z)/2,(-math.sin(angle)*y+math.cos(angle)*z)/2]
variant=copy.deepcopy(fixture)
variant['nodes'][0]['rotation']=qmul(qy(0.2),qx)
variant['nodes'][0]['scale']=[2,2,2]
variant['nodes'][1]['translation']=inverse_position(fixture['nodes'][1]['translation'])
variant['nodes'][1]['rotation']=qxinv
for node in variant['nodes'][2:]:node['translation']=[v/2 for v in node['translation']]
transformed=bytearray(data)
for accessor,components,conversion in [(hips,3,inverse_position),(turn,4,lambda q:qmul(qxinv,q))]:
    offset=views[accessors[accessor]['bufferView']]['byteOffset']
    count=accessors[accessor]['count']
    values=struct.unpack_from('<'+'f'*(count*components),transformed,offset)
    converted=[v for i in range(count) for v in conversion(values[i*components:(i+1)*components])]
    struct.pack_into('<'+'f'*len(converted),transformed,offset,*converted)
save('rotated-scaled-parent.vrma',variant,bytes(transformed))
fixture['buffers'][0]['uri']='../outside.bin'
save('external.vrma',fixture,bytes(data))
(output/'corrupt.vrma').write_text('invalid VRMA')
print(output/'anchor-and-silhouette.vrma')
