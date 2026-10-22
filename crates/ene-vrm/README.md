# ene-vrm

Bevy 0.19.1 のglTF拡張フックにVRM 1.0／VRMA 1.0を実装する、非公開のワークスペースクレートです。

glTF本体のデコード、バッファ・画像・メッシュ・スキンの生成、補間曲線、シーン複製、描画キューはBevyが担当します。このクレートはVRMC拡張の検証、ヒューマノイドの姿勢変換、表情、LookAt、ノード制約、SpringBone、MToonと輪郭を担当します。`.vrm`／`.vrma`のローダーは拡張子を接続してBevyの`GltfLoader`へ委譲します。

## API

`DefaultPlugins`の後に`VrmPlugin`を追加します。モデルは`Handle<Gltf>`で読み、`default_scene`（なければ最初のシーン）を標準の`WorldAssetRoot`で生成します。`Vrm`と`AnimationPlayer`は実際のシーンルートにあります。呼び出し側のエンティティへメタデータを移す処理はありません。

モーションも標準の`AnimationClip`です。`AssetServer::load("motion.vrma#Animation0")`で読み、`AnimationGraph`／`AnimationGraphHandle`／`AnimationPlayer`／`AnimationTransitions`で再生します。ノード番号をVRMのボーン・表情へ対応付け、Bevyの曲線をそのまま写像するので、STEP・LINEAR・CUBICSPLINE・単一キーを再サンプリングしません。正規化したクリップを複数モデルで共有できます。

- `Vrm::bone("hips")`などで、そのインスタンスのボーンを取得します。
- `vrm.meta`と`vrm.thumbnail`でVRMC_vrmのアバター名、作者、サムネイル画像、ライセンス情報（`licenseUrl`、再配布・改変許諾など）を取得します。
- `ExpressionWeights`は手動指定です。指定したキーはアニメーションより優先し、キーを削除するとアニメーションへ戻ります。
- `LookAt::target`は任意のワールド座標です。`None`ではVRMAの視線を使います。
- `SpringJointProps`の力・半径はワールド単位です。デスクトップ表示のメートル→ピクセル変換はアプリが担当します。`SpringRig::reset()`でワープ時の物理状態をリセットでき、フレーム落ちスパイク時の発散も`dt`クランプで防止されます。
- 実行順はVRM 1.0仕様どおりアニメーション→姿勢→LookAt→表情→制約→SpringBone→BevyのTransform伝播です。追加の全ワールド伝播はありません。

不正な必須拡張、参照範囲、選択シーン外のボーン、制約の循環、使える曲線のないVRMAはロードエラーになります。表情だけのVRMAも読み込めます。JSON Schema全体を再実装する検証器ではありません。

`ene-avatar`のシーン生成・失敗通知・再読み込みは[character.rs](../../apps/ene-avatar/src/character.rs)を参照してください。アプリはシーンの依存アセットの準備を待ってから`WorldAssetRoot`を付け、置換したシーンの遅延通知を無視します。

## 材質と対象範囲

`MToonMaterial`は`ExtendedMaterial<StandardMaterial, MToonExtension>`です。Bevyが作成したStandardMaterialと、標準の頂点・スキン・モーフ処理をそのまま利用します。輪郭は同じメッシュ、スキン、モーフ参照を共有する追加エンティティです。独自RenderPhaseやRenderGraphはありません。表情で変更する材質だけをインスタンスごとに複製します。

輪郭の頂点シェーダーはBevy標準の`mesh.wgsl`をimportし、その出力へVRM固有の押し出しだけを加えます。標準シェーダーの文字列置換・複製・実行時生成はありません。本体・プリパス・影の頂点処理は標準のままです。[NagaのDX12での番号補正](https://github.com/gfx-rs/wgpu/issues/4564)に合わせ、頂点・インスタンス番号を呼び出し元のエントリーポイントで読み取ってから標準関数へ渡します。

MToonの半透明描画順序は、仕様に基づき `alphaMode` が `BLEND` のときのみ `renderQueueOffsetNumber` を Bevy の `StandardMaterial::depth_bias` に反映します（`transparentWithZWrite` 時は ZWrite なしより先に描画されるようオフセットを調整）。Bevy の半透明ソートはワールド単位の距離に基づいて行われるため、モデルに極端なスケール（例: ×100 以上）を適用している場合、±1〜9 のオフセットでは意図した順序が保証されない可能性がある点に注意してください。

SpringBoneは[VRM 1.0仕様](https://github.com/vrm-c/vrm-specification/tree/master/specification/VRMC_springBone-1.0)どおり隣接ジョイントの組だけを計算します。末端は尾として使い、VRM 0.xの仮想7cmボーンを追加しません。表情処理も、バインドが指定するモーフ番号・材質項目だけを変更します。

MToonは前方描画のDirectionalLight・環境光を対象とします。VRM 0.x、FirstPerson、BodyTracking、SpringBoneの追加拡張、遅延描画、独自ファイル監視は対象外です。glTF共通機能の対応範囲はBevyに従います。

姿勢・制約・SpringBoneの数式とMToon照明の一部はbevy_vrm1（Copyright 2025 elm、MIT OR Apache-2.0）を参照しています。ライセンスは`LICENSE-bevy_vrm1-MIT`と`LICENSE-bevy_vrm1-APACHE2`に保存しています。

## 検証

```sh
cargo test --locked -p ene-vrm -p ene-avatar
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
```

GPUのスモークテストは通常のCIでは無効です。ライセンスを確認したモデルを`ENE_VRM_TEST_MODEL`、任意のモーションを`ENE_VRM_TEST_MOTION`へ指定し、`cargo test -p ene-vrm gpu_model -- --ignored --nocapture`で実行します。`ENE_VRM_CAPTURE`を指定すると透過PNGを保存します。2体の材質・スキン・輪郭の描画とシェーダーパイプライン、描画結果を検査します。

輪郭を隠した標準頂点処理の描画と比較し、輪郭が本体の外へ異常に飛び出していないことも検査します。許容幅には実際の輪郭材質だけを使います。WindowsのDX12では番号補正を外すとこの検査が失敗し、補正ありでは通ることを確認しています。
