# ene-vrm

Bevy 0.19.1 の glTF 拡張フックに VRM 1.0／VRMA 1.0 を実装する、非公開のワークスペースクレートです。

glTF本体のデコード、バッファ・画像・メッシュ・スキンの生成、補間曲線、シーン複製、描画キューはBevyが担当します。このクレートはVRMC拡張の解析、ヒューマノイドの姿勢変換、表情、LookAt、ノード制約、SpringBone、MToonと輪郭を担当します。`.vrm`／`.vrma`のローダーは拡張子を接続してBevyの`GltfLoader`へ委譲します。

## API

`DefaultPlugins`の後に`VrmPlugin`を追加します。モデルは`Handle<Gltf>`で読み、`default_scene`（なければ最初のシーン）を標準の`WorldAssetRoot`で生成します。`Vrm`と`AnimationPlayer`は実際のシーンルートにあります。

モーションも標準の`AnimationClip`です。`AssetServer::load("motion.vrma#Animation0")`で読み、`AnimationGraph`／`AnimationGraphHandle`／`AnimationPlayer`／`AnimationTransitions`で再生します。ノード番号をVRMのボーン・表情へ対応付け、Bevyの曲線をそのまま写像するので、STEP・LINEAR・CUBICSPLINE・単一キーを再サンプリングしません。正規化したクリップを複数モデルで共有できます。

- `Vrm::bone("hips")`などで、そのインスタンスのボーンを取得します。
- `vrm.meta`でVRMC_vrmのアバター名、作者、ライセンス情報（`licenseUrl`、再配布・改変許諾など）を取得します。値の内容は再検証しません。
- `ExpressionWeights`は手動指定です。指定したキーはアニメーションより優先し、キーを削除するとアニメーションへ戻ります。
- `LookAt::target`は任意のワールド座標です。`None`ではVRMAの視線を使います。
- `SpringJointProps`の力・半径はモデル空間のメートルです。デスクトップ表示のメートル→ピクセル変換はアプリが担当します。
- `SpringRig::paused`をtrueにすると固定ステップの積分を止め、最後にシミュレーションした回転を適用し続けます。制約が同じTransformを毎フレーム書いても姿勢を保持します。`SpringJointProps`は設定であり、外すとそのジョイントはSpringBoneの対象外になります。
- 実行順はアニメーション→姿勢→LookAt→表情→制約→SpringBone→BevyのTransform伝播です。追加の全ワールド伝播はありません。

必須拡張の欠落、ノード・コライダーなどの参照範囲、選択シーン外の参照、制約の循環、使える曲線のないVRMAはロードエラーになります。Bevyのアニメーションターゲットを一意に決められないVRMAもロードエラーになります。これは同一の親の下の同名ノードだけでなく、選択していないシーンのノードが同じ名前パスへ入る場合も含みます。表情だけのVRMAも読み込めます。メタ情報の内容、ヒューマノイド階層やT-pose、MToonの値域など、制作ツールが保証する仕様適合はロード時に再検証しません。JSON Schemaの検証器ではありません。

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
- VRM 0.x、BodyTracking、`VRMC_springBone`の拡張colliderは対象外です。

## 材質と対象範囲

`MToonMaterial`は`ExtendedMaterial<StandardMaterial, MToonExtension>`です。Bevyが作成したStandardMaterialと、標準の頂点・スキン・モーフ処理をそのまま利用します。輪郭は同じメッシュ、スキン、モーフ参照を共有する追加エンティティです。独自RenderPhaseやRenderGraphはありません。表情で変更する材質だけをインスタンスごとに複製します。

輪郭の頂点シェーダーはBevy標準の`mesh.wgsl`をimportし、その出力へVRM固有の押し出しだけを加えます。標準シェーダーの文字列置換・複製・実行時生成はありません。プリパス・影の頂点処理もBevy標準の`prepass.wgsl`を呼び出し、その出力のUVだけをMToonのUVアニメーション後の値へ差し替えます。フラグメントは標準のプリパス処理をそのまま使い、アルファ判定の境界だけを本体と揃えます。[NagaのDX12での番号補正](https://github.com/gfx-rs/wgpu/issues/4564)に合わせ、頂点・インスタンス番号を呼び出し元のエントリーポイントで読み取ってから標準関数へ渡します。

MToonの半透明材質は、`alphaMode`が`BLEND`のときBevy標準の半透明合成で描画します。`renderQueueOffsetNumber`による描画順の指定と`transparentWithZWrite`による深度書き込みは対象外です。Bevy 0.19.1の半透明ソートはワールド距離だけをキーにするため、キュー番号を距離への加算値で近似しても通常の表示倍率では順序を保証できません。MToonの色テクスチャはsRGB、数値テクスチャは線形で読みます。コア材質との共有でBevyが逆向きの形式を選んだ場合は、ローダーが `(texture index, color space)` ごとに画像を複製します。UVアニメーションはマスクと回転をプリパスでは頂点単位で評価するため、マスクが1三角形内で変化する場合だけ本体のフラグメント評価と厳密には一致しません。

SpringBoneは[VRM 1.0仕様](https://github.com/vrm-c/vrm-specification/tree/master/specification/VRMC_springBone-1.0)どおり隣接ジョイントの組だけを計算します。末端は尾として使い、VRM 0.xの仮想7cmボーンを追加しません。`SpringRig::paused`は積分だけを止め、最後の回転を再適用するので、ポーズ中も制約に姿勢を上書きされません。表情処理も、バインドが指定するモーフ番号・材質項目だけを変更します。バインドされたモーフは仕様どおり毎フレーム0から加算し、glTFの初期モーフウェイトは使いません。低FPS時は1/60秒の固定ステップで祖先から子孫の順に追従し、残余時間を次のフレームへ繰り越します。長時間停止は約0.25秒で打ち切ります。

MToonは前方描画のDirectionalLight・環境光を対象とします。VRM 0.x、FirstPerson、BodyTracking、SpringBoneの追加拡張、遅延描画、独自ファイル監視は対象外です。glTF共通機能の対応範囲はBevyに従います。

姿勢・制約・SpringBoneの数式とMToon照明の一部はbevy_vrm1（Copyright 2025 elm、MIT OR Apache-2.0）を参照しています。ライセンスは`LICENSE-bevy_vrm1-MIT`と`LICENSE-bevy_vrm1-APACHE2`に保存しています。

## 検証

```sh
cargo test --locked -p ene-vrm -p ene-avatar
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
```

GPUのスモークテストは通常のCIでは無効です。ライセンスを確認したモデルを`ENE_VRM_TEST_MODEL`へ指定し、`cargo test -p ene-vrm gpu_model -- --ignored --nocapture`で実行します。合成MToon/outlineテストは`cargo test -p ene-vrm synthetic_mtoon -- --ignored --nocapture`で実行でき、CIではlavapipe上で実行します。
