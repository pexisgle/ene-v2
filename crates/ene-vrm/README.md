# ene-vrm

Bevy 0.20.0 の glTF 拡張フックに VRM 1.0／VRMA 1.0 を実装する非公開クレートです。glTFのデコード、シーン・スキン・モーフ、アニメーションの補間、描画はBevyが担当し、VRMC拡張の解析と、ヒューマノイド姿勢・表情・LookAt・ノード制約・SpringBone・MToon（輪郭を含む）の適用だけを担います。`VRMC_vrm`／`VRMC_vrm_animation`／`VRMC_springBone`の型は`vrm-spec`、制約の依存順序は`petgraph`を使います。

## 使い方

`DefaultPlugins`の後に`VrmPlugin`を追加します。`.vrm`／`.vrma`は`AssetServer`で読み、モデルは標準どおり`default_scene`を`WorldAssetRoot`で生成し、モーションは`AnimationPlayer`で再生します。`Vrm`と`AnimationPlayer`は生成されたシーンルートに付きます。

- `Vrm::bone(HumanBoneName::Hips)`でボーン、`vrm.meta`で`vrm-spec`のメタ情報を取得します。
- `ExpressionWeights`／`LookAt`／`SpringRig::paused`／`SpringJointProps`で表情・視線・物理を制御します。手動で指定した表情はアニメーションに優先し、削除するとアニメーションへ戻ります。
- 実行順はアニメーション→姿勢→LookAt→表情→制約→SpringBone→Transform伝播です。

## 対象外と制限

- VRM 0.x、BodyTracking、`firstPerson`、SpringBoneの拡張collider、MToonのpoint/spot light、遅延描画、scene-less VRMA。
- モデル本体のglTF `animations`／`cameras`は読みません（VRMAでは読みます）。
- MToonの`renderQueueOffsetNumber`と`transparentWithZWrite`は対象外で、半透明はBevy標準の距離ソートと合成を使います。OITも無効です。
- MToonのテクスチャは用途別の色空間（数値用途は線形、色用途はsRGB）で読みます。コア材質と共有してBevyが逆向きの形式を選んだ場合は、`(texture index, color space)`ごとに画像を複製します。GLB埋め込み画像だけが対象です。
- 表情の`emissionColor`はglTFの`emissiveFactor`として扱い、読み込み時の`KHR_materials_emissive_strength`倍率を目標値にも適用します。
- `normalTexture.scale`はBevyのglTFローダーが未実装のため反映されません。コア材質のテクスチャごとの`KHR_texture_transform`もBevyの単一`uv_transform`に合わせて対象外です。
- JSON Schemaの完全検証はしません。必須拡張や参照範囲、制約の循環、割り当てる曲線のターゲットを一意に決められないVRMAだけをロードエラーにします。
- 仕様のデフォルトはthree-vrm互換です（`shadeColorFactor`は黒、`RangeMap.outputScale`の欠落はexpression=1／bone=10、`offsetFromHeadBone`は`[0, 0.06, 0]`）。
- 輪郭とプリパスのシェーダーは、WESLが別シェーダーのエントリーポイントを呼べないためBevy 0.20.0の標準処理をコピーしています。コピー元と自前の差分は各ファイル先頭に記載し、Bevy更新時に再同期します。

## 検証

```sh
cargo test --locked -p ene-vrm -p ene-avatar
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
```

GPUテストは`--ignored`です。合成シーンのパイプラインスモークと、スキン＋モーフ変形をCPU参照と画素比較するテストはlavapipe（CI）で、UVマスクの画素回帰はハードウェアGPUで実行します。実モデル（`ENE_VRM_TEST_MODEL`）とWindows／Wayland実機は未検証です。

姿勢・制約・SpringBoneの数式とMToon照明の一部はbevy_vrm1（MIT OR Apache-2.0、`LICENSE-bevy_vrm1-*`）を参照しています。
