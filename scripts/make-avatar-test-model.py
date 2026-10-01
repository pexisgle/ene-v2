#!/usr/bin/env python3
"""Generate an original embedded VRM 1.0 coverage fixture; no third-party assets.

Panels exercise transparent texels, MASK, BLEND (alpha 0/8/9/128/255), gaps,
nonzero morph weights, and MToon outlines. Seed-san motions cover live skinning.
"""
import argparse
import json
from pathlib import Path
import struct
import zlib


def png(width, height, pixels):
    def chunk(kind, payload):
        return struct.pack('>I', len(payload)) + kind + payload + struct.pack('>I', zlib.crc32(kind + payload))
    rows = b''.join(b'\0' + pixels[y * width * 4:(y + 1) * width * 4] for y in range(height))
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>2I5B', width, height, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b'')


def generate():
    binary = bytearray()
    views, accessors = [], []
    def blob(data, target=None):
        while len(binary) % 4:
            binary.append(0)
        view = {'buffer': 0, 'byteOffset': len(binary), 'byteLength': len(data)}
        if target:
            view['target'] = target
        views.append(view)
        binary.extend(data)
        return len(views) - 1
    def accessor(values, kind, components, bounds=False):
        view = blob(struct.pack('<' + 'f' * len(values), *values), 34962)
        item = {'bufferView': view, 'componentType': 5126, 'count': len(values) // components, 'type': kind}
        if bounds:
            item['min'] = [min(values[c::components]) for c in range(components)]
            item['max'] = [max(values[c::components]) for c in range(components)]
        accessors.append(item)
        return len(accessors) - 1
    texture = bytearray()
    for y in range(32):
        for x in range(32):
            alpha = [0, 8, 9, 128, 255][(x // 4 + y // 4) % 5]
            texture.extend([240, 100, 40, alpha])
    image = blob(png(32, 32, texture))
    mtoon = {'specVersion': '1.0', 'transparentWithZWrite': False,
             'renderQueueOffsetNumber': 0, 'shadeColorFactor': [0.7, 0.5, 0.3],
             'shadingShiftFactor': 0, 'shadingToonyFactor': 0.9, 'giEqualizationFactor': 0.9,
             'matcapFactor': [0, 0, 0], 'parametricRimColorFactor': [0, 0, 0],
             'rimLightingMixFactor': 1, 'parametricRimFresnelPowerFactor': 1,
             'parametricRimLiftFactor': 0, 'outlineWidthMode': 'worldCoordinates',
             'outlineWidthFactor': 0.025, 'outlineColorFactor': [0, 0, 0],
             'outlineLightingMixFactor': 0, 'uvAnimationScrollXSpeedFactor': 0,
             'uvAnimationScrollYSpeedFactor': 0, 'uvAnimationRotationSpeedFactor': 0}
    materials = []
    for mode in ['OPAQUE', 'MASK', 'BLEND']:
        material = {'name': mode, 'alphaMode': mode, 'doubleSided': False,
                    'pbrMetallicRoughness': {'baseColorFactor': [0.3, 0.65, 0.9, 1], 'metallicFactor': 0, 'roughnessFactor': 1},
                    'extensions': {'KHR_materials_unlit': {}, 'VRMC_materials_mtoon': dict(mtoon)}}
        if mode != 'OPAQUE':
            material['pbrMetallicRoughness']['baseColorTexture'] = {'index': 0}
        if mode == 'MASK':
            material['alphaCutoff'] = 0.5
        materials.append(material)
    primitives = []
    # A central torso and independent arms leave genuine geometric holes.
    for left, right, bottom, top, material in [(-.18, .18, 0, 2, 0), (-.65, -.3, .45, 1.7, 1), (.3, .65, .45, 1.7, 2)]:
        positions = [left,bottom,0, right,bottom,0, right,top,0, left,bottom,0, right,top,0, left,top,0]
        normals = [0, 0, 1] * 6
        uv = [0,1, 1,1, 1,0, 0,1, 1,0, 0,0]
        if material == 0:
            # A closed torso has back-facing geometry for MToon's inverted
            # hull outline; open front-facing panels alone cannot prove it.
            depth = .12
            faces = [
                ([(left,bottom,depth), (right,bottom,depth), (right,top,depth), (left,top,depth)], (0,0,1)),
                ([(right,bottom,-depth), (left,bottom,-depth), (left,top,-depth), (right,top,-depth)], (0,0,-1)),
                ([(left,bottom,-depth), (left,bottom,depth), (left,top,depth), (left,top,-depth)], (-1,0,0)),
                ([(right,bottom,depth), (right,bottom,-depth), (right,top,-depth), (right,top,depth)], (1,0,0)),
                ([(left,top,depth), (right,top,depth), (right,top,-depth), (left,top,-depth)], (0,1,0)),
                ([(left,bottom,-depth), (right,bottom,-depth), (right,bottom,depth), (left,bottom,depth)], (0,-1,0)),
            ]
            positions = [c for corners, _ in faces for i in (0,1,2,0,2,3) for c in corners[i]]
            normals = [c for _, normal in faces for _ in range(6) for c in normal]
            uv *= 6
        delta = [.08 if i % 3 == 0 else 0 for i in range(len(positions))]
        primitives.append({'attributes': {'POSITION': accessor(positions, 'VEC3', 3, True),
                                          'NORMAL': accessor(normals, 'VEC3', 3),
                                          'TEXCOORD_0': accessor(uv, 'VEC2', 2)},
                           'targets': [{'POSITION': accessor(delta, 'VEC3', 3, True)}], 'material': material})
    while len(binary) % 4:
        binary.append(0)
    document = {'asset': {'version': '2.0', 'generator': 'ene coverage fixture generator'},
                'scene': 0, 'scenes': [{'nodes': [0]}],
                'nodes': [{'name': 'Root', 'children': [1,2]}, {'name': 'Hips', 'translation': [0,1,0]}, {'name': 'CoveragePanels', 'mesh': 0}],
                'meshes': [{'name': 'AlphaGapsMorphOutline', 'primitives': primitives, 'weights': [0.75]}],
                'buffers': [{'byteLength': len(binary)}], 'bufferViews': views, 'accessors': accessors,
                'images': [{'bufferView': image, 'mimeType': 'image/png'}],
                'samplers': [{'magFilter': 9728, 'minFilter': 9728, 'wrapS': 33071, 'wrapT': 33071}],
                'textures': [{'sampler': 0, 'source': 0}], 'materials': materials,
                'extensionsUsed': ['VRMC_vrm', 'VRMC_materials_mtoon', 'KHR_materials_unlit'],
                'extensions': {'VRMC_vrm': {'specVersion': '1.0', 'humanoid': {'humanBones': {'hips': {'node': 1}}},
                                           'meta': {'name': 'ene coverage fixture', 'authors': ['ene contributors'],
                                                    'licenseUrl': 'https://opensource.org/license/mit',
                                                    'allowAntisocialOrHateUsage': False, 'allowExcessivelySexualUsage': False,
                                                    'allowExcessivelyViolentUsage': False, 'allowPoliticalOrReligiousUsage': False,
                                                    'allowRedistribution': True, 'modification': 'allowModificationRedistribution'}}}}
    encoded = json.dumps(document, separators=(',', ':')).encode()
    encoded += b' ' * (-len(encoded) % 4)
    length = 12 + 8 + len(encoded) + 8 + len(binary)
    return struct.pack('<4sII', b'glTF', 2, length) + struct.pack('<I4s', len(encoded), b'JSON') + encoded + struct.pack('<I4s', len(binary), b'BIN\0') + binary


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    args.output.write_bytes(generate())
    print(args.output)
