# ene-vrm

Bevy 0.20.0 の glTF 拡張フックに VRM 1.0／VRMA 1.0 を実装する、非公開のワークスペースクレートです。

glTF本体のデコード、バッファ・画像・メッシュ・スキンの生成、補間曲線、シーン複製、描画キューはBevyが担当します。このクレートはVRMC拡張の解析、ヒューマノイドの姿勢変換、表情、LookAt、ノード制約、SpringBone、MToonと輪郭を担当します。`VRMC_vrm`／`VRMC_vrm_animation`／`VRMC_springBone`のデータ構造には`vrm-spec`を使い、制約の依存順序は`petgraph`で解決します。`.vrm`／`.vrma`のローダーは拡張子を接続してBevyの`GltfLoader`へ委譲します。

## API

`DefaultPlugins`の後に`VrmPlugin`を追加します。モデルは`Handle<Gltf>`で読み、`default_scene`（なければ最初のシーン）を標準の`WorldAssetRoot`で生成します。`Vrm`と`AnimationPlayer`は実際のシーンルートにあります。

モーションも標準の`AnimationClip`です。`AssetServer::load("motion.vrma#Animation0")`で読み、`AnimationGraph`／`AnimationGraphHandle`／`AnimationPlayer`／`AnimationTransitions`で再生します。ノード番号をVRMのボーン・表情へ対応付け、Bevyの曲線をそのまま写像するので、STEP・LINEAR・CUBICSPLINE・単一キーを再サンプリングしません。正規化したクリップを複数モデルで共有できます。

- `Vrm::bone(HumanBoneName::Hips)`などで、そのインスタンスのボーンを取得します。
- `vrm.meta`は`vrm-spec`の`Meta`で、VRMC_vrmのアバター名、作者、ライセンス情報（`licenseUrl`、再配布・改変許諾など）を取得します。値の内容は再検証しません。
- `ExpressionWeights`は手動指定です。キーは`ExpressionName::Preset(...)`／`ExpressionName::Custom(...)`で、指定したキーはアニメーションより優先し、キーを削除するとアニメーションへ戻ります。
- `LookAt::target`は任意のワールド座標です。`None`ではVRMAの視線を使います。
- `SpringJointProps`の力・半径はモデル空間のメートルです。デスクトップ表示のメートル→ピクセル変換はアプリが担当します。
- `SpringRig::paused`をtrueにすると固定ステップの積分を止め、最後にシミュレーションした回転を適用し続けます。制約が同じTransformを毎フレーム書いても姿勢を保持します。`SpringJointProps`は設定であり、外すとそのジョイントはSpringBoneの対象外になります。
- 実行順はアニメーション→姿勢→LookAt→表情→制約→SpringBone→BevyのTransform伝播です。追加の全ワールド伝播はありません。

必須拡張の欠落、ノード・コライダーなどの参照範囲、選択シーン外の参照、制約の循環、使える曲線のないVRMAはロードエラーになります。モデルへ割り当てる曲線（ヒューマノイド・表情・視線）のアニメーションターゲットを一意に決められないVRMAもロードエラーになります。同一の親の下の同名ノードだけでなく、選択していないシーンのノードが同じ名前パスへ入る場合も、割り当てる曲線が関与すれば拒否します。割り当てない曲線同士だけが同じ名前パスを共有する場合は読み込めます。表情だけのVRMAも読み込めます。メタ情報の内容、ヒューマノイド階層やT-pose、MToonの値域など、制作ツールが保証する仕様適合はロード時に再検証しません。JSON Schemaの検証器ではありません。

## VRM仕様のデフォルトと制限

- `shadeColorFactor`のデフォルトは`[0, 0, 0]`です。MToon README・three-vrmの実装に合わせています。JSON Schemaの`default`は`[1, 1, 1]`ですが、これは非normativeなannotationとして扱います。
- `RangeMap.outputScale`が欠落している場合は、three-vrm互換で`type: expression`なら`1`、`type: bone`なら`10`を使います。VRM 1.0のJSON Schemaはこのデフォルトを定義していません。
- `offsetFromHeadBone`が欠落している場合は`[0, 0.06, 0]`を使います。
- `firstPerson`（mesh annotation / auto split）、MToonのpoint/spot lightと正確なIBL、scene-less VRMA、非必須ボーンの再配分は対象外です。
- モデル本体のglTF `animations`と`cameras`は読み込みません（VRM 1.0ではanimationsは未使用項目）。姿勢はヒューマノイドとVRMAだけで動かします。VRMAではアニメーションを読み込みます。
- MToonの半透明描画順序を指定する`renderQueueOffsetNumber`と`transparentWithZWrite`は対象外です。半透明材質はBevy標準の距離ソートと合成で描画します。
- MToonのテクスチャは用途ごとの色空間で読みます。数値用途（`shadingShiftTexture`、`uvAnimationMaskTexture`、`outlineWidthMultiplyTexture`）は線形、色用途（`shadeMultiplyTexture`、`rimMultiplyTexture`、`matcapTexture`、baseColor、emissive）はsRGBです。同じテクスチャ番号がコア材質のnormal/occlusion/metallic-roughnessと共有され、Bevyが線形形式で読み込んだ場合も、色用途にはsRGB形式の複製を渡します。GLB内の埋め込み画像だけが対象で、外部ファイルURIの画像は変換しません。
- 片眼のみのbone型LookAtはUniVRM互換でexpressionへフォールバックします。
- コアテクスチャのテクスチャごとの`KHR_texture_transform`はBevyが単一`uv_transform`しか持たないため対象外です（VRM本体仕様もrotation/texCoordを非推奨）。MToon拡張テクスチャはUV0/UV1と`texCoord`、テクスチャごとの`KHR_texture_transform`に対応します。
- glTFコア材質の解釈はBevyに従います。Bevy 0.20.0のglTFローダーは`normalTexture.scale`を未実装（`TODO: handle normal_texture.scale`）のため、MToonの法線テクスチャ強度は反映されません。上流での対応を待ちます。
- VRM 0.x、BodyTracking、`VRMC_springBone`の拡張colliderは対象外です。

## 材質と対象範囲

MToonのシェーダーはBevy 0.20のWESLで実装しています。`MToonMaterial`は`ExtendedMaterial<StandardMaterial, MToonExtension>`です。Bevyが作成したStandardMaterialと、標準のシーン・スキン・モーフ処理を利用します。MToon本体の頂点処理はBevy標準の`mesh.wesl`のままです。輪郭は同じメッシュ、スキン、モーフ参照を共有する追加エンティティです。独自RenderPhaseやRenderGraphはありません。表情で変更する材質だけをインスタンスごとに複製します。

Bevy 0.20のWESLでは別シェーダーのエントリーポイントを呼び出せないため、輪郭の頂点シェーダーは標準`mesh.wesl`の頂点処理のうちエントリーポイント（スキニング・法線/接線・可視範囲）を`outline.wesl`内に持ち、VRM固有の押し出しだけを追加します。モーフ変位は通常関数である`bevy_pbr::render::mesh::morph_vertex`を`MORPH_TARGETS`付きでインポートして再利用します。プリパスも同様に、標準`pbr_prepass.wesl`のフラグメント処理を`mtoon_prepass.wesl`内に持ち、標準頂点処理が補間したUVへMToonのマスク・スクロール・回転をフラグメント単位で適用してから、標準のアルファ判定・法線・モーション出力へ渡します。エントリーポイントとプリパスの処理はBevy 0.20.0の標準シェーダーを基準にしたコピーで、Bevy更新時に再同期が必要です。インポートする関数も名前・引数の追随が必要です。[NagaのDX12での番号補正](https://github.com/gfx-rs/wgpu/issues/4564)に合わせ、頂点・インスタンス番号を呼び出し元のエントリーポイントで読み取ってから標準関数へ渡します。

MToonの半透明材質は、`alphaMode`が`BLEND`のときBevy標準の半透明合成で描画します。`renderQueueOffsetNumber`による描画順の指定と`transparentWithZWrite`による深度書き込みは対象外です。Bevy 0.20.0の半透明ソートはワールド距離だけをキーにするため、キュー番号を距離への加算値で近似しても通常の表示倍率では順序を保証できません。MToonの色テクスチャはsRGB、数値テクスチャは線形で読みます。コア材質との共有でBevyが逆向きの形式を選んだ場合は、ローダーが `(texture index, color space)` ごとに画像を複製します。UVアニメーションはマスク・スクロール・回転を本体とプリパスの両方でフラグメント単位に評価するため、影・深度のアルファ判定も本体と一致します。MToonはOIT（`MaterialExtension::enable_oit`）を無効にしているため、OIT有効カメラでも通常の半透明パスで描画されます。

SpringBoneは[VRM 1.0仕様](https://github.com/vrm-c/vrm-specification/tree/master/specification/VRMC_springBone-1.0)どおり隣接ジョイントの組だけを計算します。末端は尾として使い、VRM 0.xの仮想7cmボーンを追加しません。`SpringRig::paused`は積分だけを止め、最後の回転を再適用するので、ポーズ中も制約に姿勢を上書きされません。表情処理も、バインドが指定するモーフ番号・材質項目だけを変更します。`emissionColor`はglTFの`emissiveFactor`として補間し、読み込み時にBevyが`emissive`へ掛けた`KHR_materials_emissive_strength`の倍率を目標値にも適用します。バインドされたモーフは仕様どおり毎フレーム0から加算し、glTFの初期モーフウェイトは使いません。低FPS時は1/60秒の固定ステップで祖先から子孫の順に追従し、残余時間を次のフレームへ繰り越します。長時間停止は約0.25秒で打ち切ります。

MToonは前方描画のDirectionalLight・環境光を対象とします。VRM 0.x、FirstPerson、BodyTracking、SpringBoneの追加拡張、遅延描画、独自ファイル監視は対象外です。glTF共通機能の対応範囲はBevyに従います。

姿勢・制約・SpringBoneの数式とMToon照明の一部はbevy_vrm1（Copyright 2025 elm、MIT OR Apache-2.0）を参照しています。ライセンスは`LICENSE-bevy_vrm1-MIT`と`LICENSE-bevy_vrm1-APACHE2`に保存しています。

## 検証

```sh
cargo test --locked -p ene-vrm -p ene-avatar
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
```

GPUのスモークテストは通常のCIでは無効です。ライセンスを確認したモデルを`ENE_VRM_TEST_MODEL`へ指定し、`cargo test -p ene-vrm gpu_model -- --ignored --nocapture`で実行します。合成MToon/outlineのパイプラインスモークは`cargo test -p ene-vrm synthetic_mtoon_uv1_outline_pipeline_smoke -- --ignored --nocapture`で実行し、CIではlavapipe上で実行します。合成シーンは1ボーン・1モーフ・接線・法線マップ・Depth/NormalPrepass・輪郭を持ち、スキニング／モーフ／法線マップ／プリパスのシェーダーvariantも生成します。UVアニメーションマスクのピクセル回帰`cargo test -p ene-vrm synthetic_mtoon_uv_alpha_shadow_matches_prepass -- --ignored --nocapture`はハードウェアGPUで実行してください。lavapipeはプリパス・フラグメントのdiscard経路でクラッシュするため、このテストはCIの対象外です。CIのGPU確認はlavapipe上の合成シーンだけです。Windowsを含む実ハードウェアGPUと、ライセンス済み実モデルでの描画はCIでは確認していません。
